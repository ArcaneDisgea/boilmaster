# Mark XIV Thermocoil Boilmaster

Web service for Final Fantasy XIV game data and asset discovery.

## Installation

### Building From Source

**Requirements**
 - [Rust](https://www.rust-lang.org/tools/install)

```
git clone https://github.com/ackwell/boilmaster
cd boilmaster
cargo run --release
```

It is recommended to edit your config in `boilmaster.toml` and change the admin username and password.

### Docker Usage

First we set up a working directory and prepare our config.
```
mkdir boilmaster
cd boilmaster
curl --create-dirs -O --output-dir config https://raw.githubusercontent.com/ackwell/boilmaster/main/boilmaster.toml
```
Then we can create a `docker-compose.yml` using the example below:
```
services:
  boilmaster:
    image: ghcr.io/arcanedisgea/boilmaster:image-creation-test
    container_name: boilmaster
    environment:
     - BM_HTTP_ADMIN_AUTH_USERNAME="CHANGE-ME"
     - BM_HTTP_ADMIN_AUTH_PASSWORD="CHANGE-ME"
    volumes:
      - type: "bind"
        source: "${PWD}/config/boilmaster.toml"
        target: "/app/boilmaster.toml"
      # Store patches persistently so we don't have to redownload them whenever we create the container.
      # Needs roughly 100gb of free space.
      - ${PWD}/patches:/app/patches
    ports:
      - 8080:8080
    restart: unless-stopped
```