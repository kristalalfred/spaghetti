# Spaghetti

Route macOS app audio to physical or virtual output devices, or to other machines over SRT, from the CLI.

Requires macOS, Rust, and Xcode Command Line Tools.

```sh
./scripts/install.sh
```

Find an app, tap its audio as a source, and route it to the default Spaghetti device:

```sh
spaghetti apps
spaghetti tap Safari
spaghetti route 1
```

Use the source ID printed by `tap`. Add `--mute` to `tap` to silence the app's original output.

Choose an output and adjust a route:

```sh
spaghetti system-devices
spaghetti route 1 '<device-uid>' --volume 0.5
spaghetti routes
spaghetti volume 1 0.8
```

Use an output UID from `system-devices` and a route ID from `routes`.

Send a source to another machine over SRT, and play what it receives. `srt://:PORT` listens, `srt://HOST:PORT` calls; one side of a link listens and the other calls:

```sh
spaghetti route 1 srt://:9000 --passphrase 'shared secret 1'
spaghetti receive srt://sending-mac.local:9000 --passphrase 'shared secret 1'
spaghetti route 2 BuiltInSpeakerDevice
```

Audio travels as AAC-LC in MPEG-TS. `--latency` sets the SRT latency in milliseconds, 200 by default.

List sources, and remove a route or a source with its routes:

```sh
spaghetti sources
spaghetti unroute 1
spaghetti unsource 1
```

CLI help: `spaghetti --help`. [MIT license](LICENSE).
