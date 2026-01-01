# kd

Small persoanl toolbox. The name means nothing - just designed to be easy to type and not clash with other tools.

## Commands TLDR

```sh
kd yt thumb resize image.png
kd gh repo apply-preferred-settings scode/foo
kd gh repo apply-preferred-settings --all
```

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
