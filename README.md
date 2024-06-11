# Johnbox

A private server implementation in Rust for modern Jackbox Games services.

**This project is not related to or endorsed by Jackbox Games, Inc.**

This project extends on the work of [InvoxiPlayGames](https://github.com/InvoxiPlayGames) over on her repo, [johnbox](https://github.com/InvoxiPlayGames/johnbox)

## Supported Software

### Tested known working games:

* The Jackbox Party Pack 2
* The Jackbox Party Pack 3
* The Jackbox Party Pack 4
* The Jackbox Party Pack 5
    * Mad Verse City (Requires TTS to be set up)
* The Jackbox Party Pack 6
* The Jackbox Party Pack 7
* The Jackbox Party Pack 8
* The Jackbox Party Pack 9
* The Jackbox Party Pack 10
    * FixyText (does not work, and is probably a wontfix)
* Drawful 2 International

### Tested known non-working games:

* The Jackbox Party Pack
    * Disregards the `jbg.config.jet` files, and just contacts Jackbox's servers anyway
* FixyText
    * Requires complex API for collaborative text editing, which is not implemented and difficult to reverse engineer.

## Unimplemented features

* Room passcodes
* Audiences (WIP)
    * ATM, audience seems to work for all games, though I haven't tested audience with *every* game yet.
    - [x] Party Pack 10
    - [ ] Party Pack 9
    - [ ] Party Pack 8
    - [ ] Party Pack 7
    - [ ] Party Pack 6
    - [ ] Party Pack 5
    - [ ] Party Pack 4
    - [ ] Party Pack 3
    - [ ] Party Pack 2
* Moderation features

## Usage

### Configuration

The configuration file is located in `config.toml`, and an example configuration is already in that file. Make sure you edit this before you run the server.

You should also edit the `jbg.config.jet` file in each minigame folder. and set the `serverUrl` parameter to the accessible host you set in `config.toml` (set the `joinUrl` parameter to change the URL displayed by the game to something different)

### Running

Building and running is relatively simple. Just install Rust and run

```shell
cargo run --release
```

Or look in [releases](releases) for a binary.

### TTS

Mad Verse City requires a tts endpoint in order to generate the audio for the raps. In native mode, this can be done using the [piper](https://github.com/rhasspy/piper) project. First set that up, then go [here](https://github.com/rhasspy/piper/blob/master/VOICES.md) and download the voices and their associated config files to the `voices_path` (Note that the expected name for config files is {voice}.onnx.json, and that it's not named that already). Voices are chosen at random from the `voices_path`, but they will be consistent for each player. You will also need [ffmpeg](https://ffmpeg.org/) installed.

### Caching games

There are shell scripts in this project that serve to cache game assets in the server's cache that can't be retrieved in one playthrough.

You can run each script with your `steam/steamapps/common` directory, and the server's accessible host (HTTPS required) as arguments.

The server should be running when you run these scripts.

```shell
sh update_dodoremi.sh "$HOME/.steam/steam/steamapps/common" "192.168.1.10"
```

- `update_dodoremi.sh`
    - Dependencies:
        - find
        - jq
        - parallel
        - curl
    - Caches the sounds for every instrument that can be loaded in the game
- `update_junktopia.sh`
    - Dependencies:
        - find
        - parallel
        - curl
    - Caches every item that can be bought in the game
- `update_timejinx.sh`
    - Dependencies:
        - find
        - parallel
        - curl
    - Caches every impostor image in the game
