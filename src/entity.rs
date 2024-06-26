use std::{
    fmt::Debug,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicI64},
        Arc,
    },
};

use base64::Engine;
use color_eyre::eyre::{self, ContextCompat};
use indexmap::IndexMap;
use ringbuf::{traits::Consumer, Cons};
use serde::{de::Error, ser::SerializeSeq, Deserialize, Serialize, Serializer};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke, Transform};
use tokio::io::Interest;
use yrs::{sync::Awareness, ReadTxn, StateVector, Transact};

use crate::{acl::Role, ecast::ws::JBResult};

use super::acl::Acl;

#[derive(Serialize, Debug)]
pub struct JBEntity(pub JBType, pub JBObject, pub JBAttributes);

impl JBEntity {
    pub fn as_result<'a>(&'a self) -> JBResult<'a> {
        match self.0 {
            JBType::Text => JBResult::Text(&self.1),
            JBType::Number => JBResult::Number(&self.1),
            JBType::Object => JBResult::Object(&self.1),
            JBType::Doodle => JBResult::Doodle(&self.1),
            JBType::TextMap => JBResult::TextMap(&self.1),
            JBType::AudienceGCounter => JBResult::AudienceGCounter(&self.1),
            JBType::AudiencePnCounter => JBResult::AudiencePnCounter(&self.1),
            JBType::AudienceCountGroup => JBResult::AudienceCountGroup(&self.1),
            JBType::AudienceTextRing => JBResult::AudienceTextRing(&self.1),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JBObject {
    pub key: String,
    #[serde(flatten)]
    #[serde(default)]
    pub val: JBValue,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct JBCountGroup {
    pub choices: IndexMap<String, AtomicI64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_votes: Option<i64>,
}

impl<'de> Deserialize<'de> for JBCountGroup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Debug)]
        struct JBCreateCountGroup {
            #[serde(default)]
            #[serde(alias = "choices")]
            options: Vec<serde_json::Value>,
            #[serde(rename = "maxVotes")]
            max_votes: Option<i64>,
        }

        let count_group_opts = JBCreateCountGroup::deserialize(deserializer)?;

        let mut count_group = JBCountGroup {
            max_votes: count_group_opts.max_votes,
            ..Default::default()
        };
        for opt in count_group_opts.options {
            count_group.choices.insert(
                match opt {
                    serde_json::Value::Null => format!("null"),
                    serde_json::Value::Bool(b) => format!("{}", b),
                    serde_json::Value::Number(n) => format!("{}", n),
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Array(_) => {
                        return Err(D::Error::invalid_value(
                            serde::de::Unexpected::Seq,
                            &"A valid count-group choice",
                        ))
                    }
                    serde_json::Value::Object(_) => {
                        return Err(D::Error::invalid_value(
                            serde::de::Unexpected::Map,
                            &"A valid count-group choice",
                        ))
                    }
                },
                0.into(),
            );
        }
        Ok(count_group)
    }
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum JBValue {
    Player {
        #[serde(flatten)]
        val: JBPlayerValue,
        version: u32,
        from: AtomicI64,
        #[serde(skip_serializing_if = "JBRestrictions::is_default")]
        restrictions: JBRestrictions,
    },
    Audience(JBAudienceValue),
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum JBPlayerValue {
    Text {
        val: String,
    },
    Number {
        val: f64,
    },
    Object {
        val: serde_json::Map<String, serde_json::Value>,
    },
    Doodle {
        val: JBDoodle,
    },
    TextMap {
        #[serde(serialize_with = "serialize_root")]
        root: Awareness,
    },
    None {
        val: Option<()>,
    },
}

fn serialize_root<S>(map: &Awareness, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    base64::prelude::BASE64_STANDARD
        .encode(map.doc().transact().encode_diff_v1(&StateVector::default()))
        .serialize(serializer)
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum JBAudienceValue {
    AudiencePnCounter {
        count: AtomicI64,
    },
    AudienceGCounter {
        count: AtomicI64,
    },
    AudienceCountGroup(JBCountGroup),
    /// If a value of this type is shown in the TUI, it will prevent values
    /// from reaching the host. This is a bug that I don't really care too
    /// much about.
    AudienceTextRing {
        elements: AudienceTextRing,
    },
    None {
        val: Option<()>,
    },
}

#[derive(Clone)]
pub struct AudienceTextRing {
    pub buffer: Arc<ringbuf::HeapRb<String>>,
    pub comments_per_poll: usize,
}

impl Debug for AudienceTextRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudienceTextRing").finish_non_exhaustive()
    }
}

impl Serialize for AudienceTextRing {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq_ser = serializer.serialize_seq(None)?;

        let mut consumer = Cons::new(Arc::clone(&self.buffer));

        for item in consumer.pop_iter().take(self.comments_per_poll) {
            seq_ser.serialize_element(&item)?;
        }

        seq_ser.end()
    }
}

impl Default for JBPlayerValue {
    fn default() -> Self {
        Self::None { val: None }
    }
}

impl Default for JBAudienceValue {
    fn default() -> Self {
        Self::None { val: None }
    }
}

#[derive(Serialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum JBType {
    Text,
    Number,
    Object,
    Doodle,
    #[serde(rename = "text-map")]
    TextMap,
    #[serde(rename = "audience/g-counter")]
    AudienceGCounter,
    #[serde(rename = "audience/pn-counter")]
    AudiencePnCounter,
    #[serde(rename = "audience/count-group")]
    AudienceCountGroup,
    #[serde(rename = "audience/text-ring")]
    AudienceTextRing,
}

impl JBType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Object => "object",
            Self::Doodle => "doodle",
            Self::TextMap => "text-map",
            Self::AudiencePnCounter => "audience/pn-counter",
            Self::AudienceGCounter => "audience/g-counter",
            Self::AudienceCountGroup => "audience/count-group",
            Self::AudienceTextRing => "audience/text-ring",
        }
    }
}

impl FromStr for JBType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "number" => Ok(Self::Number),
            "object" => Ok(Self::Object),
            "doodle" => Ok(Self::Doodle),
            _ => Err(()),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq)]
pub struct JBRestrictions {
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(rename = "type")]
    pub data_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub increment: Option<f64>,
}

impl JBRestrictions {
    fn is_default(&self) -> bool {
        JBRestrictions::default().eq(self)
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JBAttributes {
    pub locked: AtomicBool,
    #[serde(skip)]
    pub acl: Vec<Acl>,
}

impl Default for JBAttributes {
    fn default() -> Self {
        Self {
            locked: false.into(),
            acl: Acl::default_vec(),
        }
    }
}

impl JBAttributes {
    pub fn perms(&self, role: Role, id: i64) -> Option<Interest> {
        if role == Role::Host {
            return Some(Interest::READABLE | Interest::WRITABLE);
        }

        let mut perms: Option<Interest> = None;

        for principle in self.acl.iter() {
            if principle.principle.matches(role, id) {
                *perms.get_or_insert(principle.interest) |= principle.interest;
            }
        }

        perms
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct JBDoodle {
    #[serde(default)]
    pub colors: Vec<csscolorparser::Color>,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub max_points: usize,
    #[serde(default)]
    pub max_layer: usize,
    #[serde(default)]
    pub size: JBSize,
    pub weights: Option<Vec<u32>>,
    #[serde(default)]
    pub lines: Vec<JBLine>,
}

impl JBDoodle {
    pub fn render(&self) -> eyre::Result<Pixmap> {
        let mut layers = vec![
            Pixmap::new(self.size.width, self.size.height).wrap_err_with(
                || format!("Failed to create Pixmap of size {:?}", self.size)
            )?;
            self.max_layer.max(1)
        ];

        for line in self.lines.iter() {
            if !line.points.is_empty() {
                let layer = layers.get_mut(line.layer).with_context(|| {
                    format!(
                        "No such layer {} out of {} layers",
                        line.layer, self.max_layer
                    )
                })?;
                let mut path = PathBuilder::new();
                let mut points = line.points.iter();
                if let Some(move_to) = points.next() {
                    path.move_to(move_to.x, move_to.y);

                    for point in points {
                        path.line_to(point.x, point.y);
                    }

                    let path = path.finish().with_context(|| {
                        format!("Failed to create path from points: {:?}", line.points)
                    })?;
                    let line_color = line.color.to_rgba8();
                    let paint = Paint {
                        shader: tiny_skia::Shader::SolidColor(Color::from_rgba8(
                            line_color[0],
                            line_color[1],
                            line_color[2],
                            line_color[3],
                        )),
                        anti_alias: true,
                        ..Default::default()
                    };
                    let stroke = Stroke {
                        width: line.weight * 2.0,
                        line_cap: tiny_skia::LineCap::Round,
                        ..Default::default()
                    };
                    layer.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
        }

        let mut layers_iter = layers.iter_mut();

        let first_layer = layers_iter.next().unwrap();
        for layer in layers_iter {
            first_layer.draw_pixmap(
                0,
                0,
                layer.as_ref(),
                &tiny_skia::PixmapPaint {
                    opacity: 1.0,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }

        Ok(layers.swap_remove(0))
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct JBLine {
    color: csscolorparser::Color,
    weight: f32,
    layer: usize,
    points: Vec<JBPoint>,
    #[serde(default)]
    pub index: usize,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
struct JBPoint {
    x: f32,
    y: f32,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct JBSize {
    width: u32,
    height: u32,
}
