# Installing this Navette

No root needed. From this folder:

```
install -Dm755 navette ~/.local/bin/navette
```

On the remote machine (the work laptop) only, after `navette pair <home:port>` has succeeded:

```
install -Dm644 navette-agent.service ~/.config/systemd/user/navette-agent.service
systemctl --user daemon-reload
systemctl --user enable --now navette-agent
```

`navette --version` tells which version this is. The full guide (update, removal, pairing) is
`docs/install.md` in the repository.
