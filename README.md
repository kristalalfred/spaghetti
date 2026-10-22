# Spaghetti

Route macOS app audio to physical or virtual output devices from the CLI.

Requires macOS, Rust, and Xcode Command Line Tools.

```sh
./scripts/install.sh
```

Find an app, tap its audio, and route it to the default Spaghetti device:

```sh
spaghetti apps
spaghetti tap Safari
spaghetti route 1
```

Use the tap ID printed by `tap`. Add `--mute` to `tap` to silence the app's original output.

Choose an output and adjust a route:

```sh
spaghetti system-devices
spaghetti route 1 '<device-uid>' --volume 0.5
spaghetti routes
spaghetti volume 1 0.8
```

Use an output UID from `system-devices` and a route ID from `routes`.

Remove a route and its tap:

```sh
spaghetti unroute 1
spaghetti untap 1
```

CLI help: `spaghetti --help`. [MIT license](LICENSE).
