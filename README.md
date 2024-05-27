# Johnbox

A private server implementation in Rust for modern Jackbox Games services (Ecast / API v2).

**This project is not related to or endorsed by Jackbox Games, Inc.**

This project extends on the work of [InvoxiPlayGames](https://github.com/InvoxiPlayGames) over on her repo, [johnbox](https://github.com/InvoxiPlayGames/johnbox)

## Supported Software

### Tested known working games:

* The Jackbox Party Pack 2 (Audience manually disabled)
* The Jackbox Party Pack 3 (Audience manually disabled)
* The Jackbox Party Pack 4 (Audience manually disabled)
* The Jackbox Party Pack 5 (Audience manually disabled)
* The Jackbox Party Pack 6 (Audience manually disabled)
    * Not tested, but I can't imagine why it wouldn't work
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
* Audiences
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
