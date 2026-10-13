# The link between the two machines

The remote machine (the managed laptop) dials the home machine over the LAN; home listens on
its LAN address only. They trust each other once paired with a PIN, then talk over mutual
TLS 1.3, each side pinning the other's certificate.

## Commands

At home, on the LAN address and a free port of your choice (the examples use invented
addresses: home `192.0.2.10`, the laptop `192.0.2.20`):

```
navette pair --listen 192.0.2.10:7420     # shows a six-digit PIN, waits 120 s
```

On the remote machine:

```
navette pair 192.0.2.10:7420              # asks for the PIN
```

Then `navette serve` at home (it listens on the address used to pair; `--listen` changes it)
and `navette agent` on the remote machine, as a systemd user service
(`dist/navette-agent.service`, how to install it is written at its top).

Both commands keep their files in `$XDG_CONFIG_HOME/navette` (`--config-dir` or
`NAVETTE_CONFIG_DIR` to move it): `identity.key` and `identity.crt` (this machine's Navette
identity) and `paired.conf` (the peer's certificate fingerprint, its role and the address),
all 0600 in a 0700 directory. Pairing again replaces `paired.conf`; to change a machine's
identity, remove its `identity.*` files and pair both machines again.

## The home firewall

Pairing and the link use the same port, so home needs one rule. With Omarchy's `ufw`:

```
sudo ufw allow in proto tcp from 192.0.2.20 to 192.0.2.10 port 7420 comment navette
```

Nothing changes on the remote machine: it only dials out, and with its corporate VPN up its
LAN traffic to home still goes directly over the local network.

## Pairing protocol

Written at the top of [`src/pair.rs`](../src/pair.rs): SPAKE2 on the PIN, then each side sends
its certificate fingerprint under an HMAC of the shared key and the transcript, and records its
peer only after checking the other's.

## Threat model

- **A machine on the LAN without the PIN** cannot pair: home accepts one connection per
  pairing session and SPAKE2 gives it one guess out of a million, with nothing to test offline.
  It cannot open the link either: home ends the TLS handshake for any certificate but the
  paired one, before any Navette message is read, and the remote refuses any home but the
  paired one.
- **Someone listening on the LAN** sees TLS 1.3 traffic, and during pairing only SPAKE2
  messages and certificate fingerprints, none of which is secret.
- **Denial of service** stays possible from the LAN: a host can take the one pairing attempt
  (pair again) or open connections that home drops after 10 s without a handshake.
- **What is not covered**: a compromised machine. Whoever can read a machine's
  `identity.key` can pose as it; the answer is a new identity and a new pairing. The address
  home listens on is the one it is given: Navette refuses a wildcard (`0.0.0.0`, `::`) but does
  not check that the address is on the LAN.
- **What never crosses**: no work credential goes to the home machine and no home credential
  to the laptop. The Navette identity is neither, and is used for nothing but this link.
