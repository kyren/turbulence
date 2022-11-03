## [0.4]
- Don't "nagle" in the reliable channel, *require* flush calls to ensure data is
  sent.
- [API Change]: Change length limits in message channels to be uniformly `u16`
  and use the type system to express maximum values rather than constants.
- Fix panics in reliable bincode channel with messages near upper limit due to
  improper buffer size.
- Document that all async methods are supposed to be cancel safe.

## [0.3]
- Fix the message_channels test to be less confusing, this is very important as
  it is currently the best (hah) example.
- Make `BufferPacketPool` derive Copy if the type it wraps is Copy.
- Simplify `Runtime` trait to not require an explicit `Interval`.
  `Runtime::Delay` wasn't even *used* prior to this, but it is the only timing
  requirement now and has been renamed to `Sleep` to match tokio 0.3. Neither
  tokio nor smol allocate as part of creating a `Sleep` / `Timer`, so having an
  explicit `Interval` is not really necessary to avoid e.g. allocation, and the
  way tokio's `Interval` works was not ideal anyway and we shouldn't rely on
  how it is implemented.

## [0.2]
- Correctness fixes for unreliable message lengths
- Performance improvements for bincode message serialization
- Avoid unnecessary calls to SendExt::send
- Performance improvements and fixes for internal `event_watch` events channel.
- [API Change]: Update to bincode 1.3, no longer using the deprecated bincode API
- [API Change]: Return `Result` in `MessageChannels` async methods on
  disconnection, panicking is never appropriate for a network error. Instead,
  the panicking version of methods in `MessageChannels` *only* panic on
  unregistered message types.

## [0.1.1]
- Small bugifx for unreliable message channels, don't error with
  `SendError::TooBig` when the message will actually fit.

## [0.1.0]
- Initial release
