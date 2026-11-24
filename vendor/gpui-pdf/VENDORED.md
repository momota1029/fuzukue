# gpui-pdf 0.7.0

This copy of [zorite's gpui-pdf](https://github.com/packetThrower/zorite)
is based on the gpui-pdf 0.7.0 source package on crates.io.
See [LICENSE](LICENSE) for the original copyright and MIT terms.

The manifest uses gpui-pre 0.3.12, and the viewer passes the GPUI app context
to the modifier check. Local viewer changes include click and text selection
events for the host, optional outline visibility, viewport-sized keyboard
scrolling, and preserving the viewport when replacing the PDF.
