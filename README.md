# Johnbox

A private server implementation in Rust for modern Jackbox Games services (Ecast / API v2).

**This project is not related to or endorsed by Jackbox Games, Inc.**

## Supported Software

### Tested known working games:

* The Jackbox Party Pack 7
    * Quiplash 3
    * Champ'd Up
    * Blather Round
    * Talking Points
    * The Devils and the Details (requires Audience explicitly disabled)
* The Jackbox Party Pack 8 (all games)
    * Job Job (requires Audience explicitly disabled)
* The Jackbox Party Pack 9 (all games)
* The Jackbox Party Pack 10 (all games)
    * FixyText (does not work, and is probably a wontfix)
* Drawful 2 International

### Tested known non-working games:

* The Jackbox Party Pack 8
    * Job Job (UI on the frontend is bugged, can not progress)
* Quiplash 2 InterLASHional
    * Quiplash 2 InterLASHional only uses API v2 for gathering server info. Rooms are still handled via blobcast.
* The Jackbox Party Pack 6
    * All games in Party Pack 6 only use API v2 for gathering server info. Rooms are still handled via blobcast.
* FixyText
    * Requires complex API for collaborative text editing, which is not implemented and difficult to reverse engineer.
* All games prior use Blobcast / API v1 (likely not the true names), which uses socketio for WebSockets and is currently not supported. 

## Unimplemented features

* Object security
* Room passcodes
* Audiences
* Moderation features
* Blobcast / API v1(? what is the real name)

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

If you don't have an internet connection when running, and you have some games cached, try running in offline mode

```shell
cargo run --release -- --offline
```

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
