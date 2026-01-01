# kd

Small persoanl toolbox. The name means nothing - just designed to be easy to type and not clash with other tools.

## Logging

Default log level is INFO.

| Flag | Level |
|------|-------|
| `-v` | DEBUG |
| `-vv` | TRACE |
| `-q` | WARN |
| `-qq` | ERROR |
| `-qqq` | OFF |

```sh
kd -v yt thumb resize image.png   # debug output
kd -qq yt thumb resize image.png  # errors only
```
