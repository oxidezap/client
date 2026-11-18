# Media Foundation frame rates

Vendored from `nokhwa-bindings-windows` 0.4.6. Media Foundation packs the
numerator in the high half and the denominator in the low half of its frame
rate attribute. The crate reported the denominator as the actual rate and
discarded fractional rates from its inventory. Mode selection also matched
the advertised range endpoints without applying the matching ratio.

Keep the exact ratio on the selected Media Foundation type and round it only
when exposing the integer FPS used by Nokhwa. Remove the patch once the upstream
crate negotiates and reports rates correctly.
