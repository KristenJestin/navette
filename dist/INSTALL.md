# Installing this Navette

No root needed. From this folder:

```
./install.sh
```

It installs `~/.local/bin/navette`, the icon its notifications show, and the systemd user units
`navette.service` and `navette-agent.service`, then reloads the user manager. It enables and
starts nothing, and running it again gives the same result.

Then, on each machine, pair once (`navette pair --listen <LAN-address:port>` at home, which shows
a PIN; `navette pair <home-address:port>` on the remote machine, which asks for it) and start the
one unit, which runs what the pairing says (`serve` at home, `agent` on the remote machine):

```
systemctl --user enable --now navette
```

`navette-agent.service` is kept for one minor version: `navette.service` replaces it.

## By hand

The same, without the script:

```
install -Dm755 navette ~/.local/bin/navette
install -Dm644 navette.svg ~/.local/share/icons/hicolor/scalable/apps/navette.svg
install -Dm644 navette.service ~/.config/systemd/user/navette.service
systemctl --user daemon-reload
```

`navette --version` tells which version this is. The full guide (update, removal, pairing) is
`docs/install.md` in the repository.
