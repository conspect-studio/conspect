# Runbook: Google Cast (cast outputs)

Design: [ADR-0111](../decisions/ADR-0111-cast-output.md), [cast-output.md](../architecture/cast-output.md).

## 1. Set up

1. Declare one cast output per picture you want on Cast devices at the same time (outputs change with a restart):

   ```toml
   [[outputs]]
   kind = "cast"
   id = "cast-1"
   # On a canvas larger than 1920x1080 give the output its own picture (costs one encoder):
   # [outputs.format]
   # width = 1920
   # height = 1080
   ```

   A canvas faster than 30 fps cannot carry a cast output yet (#343); validation says so.
2. The host name must resolve on public DNS (Cast devices use their own resolvers and ignore LAN DNS) and serve a
   certificate chain the device's firmware trusts, on port 443.
3. Receiver application id. The Conspect receiver is registered as application `8E05190C`
   (`https://cast.conspect.studio/`); set `MULTIVIEW_CAST_APP_ID=8E05190C` (or `[control] cast_app_id = "8E05190C"`).
   Until that application is published, register each test device's serial number in the Google Cast SDK
   Developer Console (https://cast.google.com/publish) under that application and reboot the device afterwards.
   Air-gapped or own registration: register a Custom Receiver with URL `https://<host>/cast/receiver/` (the engine
   serves the same files) and configure that id instead. Without an id the web UI uses Google's Default Media
   Receiver: the stream plays, with Google's buffering defaults, no Conspect screen and no receiver diagnostics.
4. Engine-side casting ("Cast from the engine", ADR-M011) also needs `[control] cast_media_base` set to the
   engine's public `https://` base, and the `devices-net` + `discovery` features (in the shipped presets).

## 2. Cast

Web UI, Cast page (Chrome or Edge): choose what to show, press Cast, choose the device. The page shows the
receiver's state (Connecting, Buffering, Playing with resolution and seconds behind live, or an error sentence).
"Show something else" changes the picture without a new cast. Stop ends the cast and revokes its link.
When the engine is on the same network as the device, "Cast from the engine" lists the devices it discovered.

## 3. When it does not play

Read `GET /api/v1/cast/config` and the stream (`GET /api/v1/cast/streams/{id}`), or the Cast page's detail line.

| What you see | Meaning | Do |
| --- | --- | --- |
| The TV shows the Cast spinner and never the Conspect screen | The TV never loaded the receiver page (`https://cast.conspect.studio/`, or `receiver_last_fetch` stays empty for a self-hosted receiver): DNS, firewall, or the device is not registered for an unpublished app | Open the receiver URL from a phone on the TV's network. Check the device serial in the Console and reboot the device. Check the certificate chain (section 4) |
| Receiver page fetched, stream `fetches.playlists = 0` | The receiver did not request the stream: the LOAD was rejected (wrong origin or path) | Receiver log (section 5); the page's host must be the host the browser is on |
| Playlists fetched, no `playing` report, report `error` set | Playback fault | The report's sentence and codes (`shaka 1001` = HTTP status, `3016` = decode) |
| "The cast stream has ended or its link expired" on the TV | The token was revoked (Stop, or a take moved the cast output to a feed the cast's owner may not see) or expired (60 s without a playlist fetch, or 12 h) | Start the cast again; `GET /api/v1/cast/streams/{id}` shows `cast-stream-revoked` and the reason |
| `409 cast-capacity` | Every cast output is in use with other pictures | Stop a cast or declare another cast output |
| `409 capacity` (routing) | No encoder free for that picture | Stop another routed feed |
| Cast output shows `last_error` about its directory | `dir` holds files the engine did not write | Point `dir` at an empty directory or remove `dir` |

## 4. Certificate check

```sh
echo | openssl s_client -connect <host>:443 -servername <host> -showcerts 2>/dev/null | grep -E ' s:| i:|NotAfter'
```

A Cast device validates the chain against the roots in its firmware. If a device never fetches (first row of the
table) while a phone on the same network can open the receiver page, issue the host's certificate from a chain
that ends at a long-established root and try again.

## 5. Receiver log on the device

The device must be registered in the Console (section 1.3). In Chrome on a computer on the same network open
`chrome://inspect`, wait for the device under "Remote Target", press "inspect", open the Console and filter on
`[conspect-cast]`. Every decision of the receiver is logged there (accepted or rejected LOAD, state changes,
errors with codes, retries, token checks). A token is never logged beyond its first 6 characters.
To show Google's debug overlay on the TV, start the cast with `customData.debug = true` (the Cast page has a
"Receiver debug overlay" switch for admins).

## 6. Verify a stream without a device

```sh
cargo run -p multiview-output --example cast_validate -- <state dir>/cast/<id>     # while a cast is active
tools/cast-proof/run.sh                                                             # engine + Shaka + receiver SDK in headless Chrome
```
