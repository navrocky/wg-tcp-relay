# Cloak transport

[Cloak](https://github.com/cbeuw/Cloak) disguises a TCP stream as ordinary TLS traffic to a
real website. Unauthenticated visitors and active probes are served the real website, which
makes the tunnel hard to detect and block with DPI. Use it instead of SSH when SSH itself is
throttled or blocked.

The layout is the same as with an SSH forward: the relay client on the entry server connects
to `127.0.0.1:5000`, and Cloak carries that connection to the relay server on the exit server.
The exit server's `443/tcp` must be reachable from the entry server.

```
entry server                                              exit server
wg-tcp-relay client -> ck-client :5000  ═══ TLS :443 ═══>  ck-server -> wg-tcp-relay server 127.0.0.1:5000
```

## Exit server: `ck-server`

Generate keys with `ck-server -key` (prints a public and a private key) and a user id with
`ck-server -uid`:

```jsonc
// ck-server.json
{
  "ProxyBook": { "wg": ["tcp", "127.0.0.1:5000"] },
  "BindAddr": [":443"],
  "RedirAddr": "www.example.com",      // real site shown to everyone else
  "PrivateKey": "<private key>",
  "BypassUID": ["<uid>"]
}
```

```sh
ck-server -c ck-server.json
```

## Entry server: `ck-client`

```jsonc
// ck-client.json
{
  "Transport": "direct",
  "ProxyMethod": "wg",                 // name from ProxyBook
  "EncryptionMethod": "plain",         // WireGuard is already encrypted
  "UID": "<uid>",
  "PublicKey": "<public key>",
  "ServerName": "www.example.com",
  "BrowserSig": "chrome",
  "NumConn": 4,
  "StreamTimeout": 300
}
```

```sh
ck-client -c ck-client.json -s exit.example.com -p 443 -l 5000
wg-tcp-relay client --listen 0.0.0.0:51820 --connect 127.0.0.1:5000
```

Notes:

- `EncryptionMethod: plain` avoids encrypting twice. The data is already WireGuard ciphertext.
- `NumConn` sets how many TLS connections Cloak spreads streams over.
- `StreamTimeout` must be longer than the relay's ping interval (5 s), which keeps the stream
  busy anyway.
- Cloak's options change between versions: check its README for the version you install.
