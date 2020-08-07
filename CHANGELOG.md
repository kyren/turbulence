## [0.2]
- Correctness fixes for unreliable message lengths
- Performance improvements for bincode message serialization
- Avoid unnecessary calls to SendExt::send
- Performance improvements and fixes for internal `event_watch` events channel.
- [API Change]: Update to bincode 1.3, no longer using the deprecated bincode API
- [API Change]: Return `Result` in `MessageChannels` async methods on
  disconnection, panicking is never appropriate for a network error.  Instead,
  the panicking version of methods in `MessageChannels` *only* panic on
  unregistered message types.

## [0.1.1]
- Small bugifx for unreliable message channels, don't error with
  `SendError::TooBig` when the message will actually fit.

## [0.1.0]
- Initial release
