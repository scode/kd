# SPEC.md

This file records intentional behavior that is easy to mistake for a bug during review.

## ImageMagick-dependent tests

The image resizing tests may skip themselves when the `magick` command is unavailable.

ImageMagick is still a runtime requirement for image operations. The skip exists so ordinary Rust validation can run in
environments that have the Rust toolchain but not the external image-processing binary installed. A test environment
that needs to prove end-to-end thumbnail behavior must install ImageMagick and run the same tests with `magick`
available on `PATH`.

This means a passing `cargo test` run without ImageMagick proves the pure Rust code still compiles and its
non-ImageMagick helpers still behave correctly. It does not prove that thumbnail resizing works end to end.
