# glib 0.18.5 local patch

Source: crates.io glib 0.18.5 (MIT; LICENSE is retained).
Backport: https://github.com/gtk-rs/gtk-rs-core/pull/1343
Upstream merge: 05dff0ee696f9bcd8617cd48c4b812d046d440cb
Advisory: RUSTSEC-2024-0429 / GHSA-wrw7-89jp-8q8g.

Only the two lines in VariantStrIter::impl_get change: declare the output pointer mutable and pass a mutable reference to GLib. The package version and 0.18 dependency interface remain unchanged. Version-only scanners may still report the advisory; it is not suppressed.

Validation: run the upstream variant_iter tests with optimization against this source. Remove this override when the GTK dependency chain accepts a release containing the fix.
