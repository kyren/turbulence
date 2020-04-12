# turbulence

*We'll get there, but it's gonna be a bumpy ride.*

---

[![Build Status](https://img.shields.io/circleci/project/github/kyren/turbulence.svg)](https://circleci.com/gh/kyren/turbulence)
[![Latest Version](https://img.shields.io/crates/v/turbulence.svg)](https://crates.io/crates/turbulence)
[![API Documentation](https://docs.rs/turbulence/badge.svg)](https://docs.rs/turbulence)


Multiplexed, optionally reliable, async, transport agnostic, reactor agnostic
networking library for games.

This library does not actually perform any networking itself or interact with
platform networking APIs in any way, it is instead a way to take some kind of
*unreliable* and *unordered* transport layer that you provide and turn it into a
set of independent networking channels, each of which can optionally be made
*reliable* and *ordered*.

The best way right now to understand what this library is useful for probably to
look at the [MessageChannels test](tests/message_channels.rs).  This is the
highest level, simplest API provided: it allows you to define N message types
serializable with serde, define each individual channel's networking settings,
and then gives you a set of handles for pushing packets into and taking packets
out of this `MessageChannels` interface.  The user is expected to take outgoing
packets and send them out over UDP (or similar), and also read incoming packets
from UDP (or similar) and pass them in.  The only reliability requirement for
using this is that if a packet is received from a remote, it must be intact and
uncorrupted, but other than this the underlying transport does not need to
provide any reliability or order guarantees.  The reason that no corruption
check is performed is that many transport layers already provide this for free,
so it would often not be useful for `turbulence` to do that itself.  Since there
is no requirement for reliability, simply dropping incoming packets that do not
pass a consistency check is appropriate.

This library is structured in a way that provides a lot of flexibility but does
not do very much to help you actually get a network connection set up between a
game server and client.  Setting up a UDP game server is a complex task, and
this library is designed to help with one *piece* of this puzzle.

---

### What this library actually does

`turbulence` currently contains two main protocols and builds some conveniences on top of them:

1) It has an unreliable, unordered messaging protocol that takes in messages that
   must be less than the size of a packet and coalesces them so that multiple
   messages are sent per packet.  This is by far the simpler of the two
   protocols, and is appropriate for per-tick updates for things like position
   data, where resends of old data are not useful.
   
2) It has a reliable, ordered transport with flow control that is similar to
   TCP, but much simpler and without automatic congestion control.  Instead of
   congestion control, the user specifies the target packet send rate as part of
   the protocol settings.
   
`turbulence` then provides on top of these:

3) Reliable and unreliable channels of `bincode` serialized types.

4) A reliable channel of `bincode` serialized types that are automatically
   coalesced and compressed.
   
And then finally this library also provides an API for multiplexing multiple
instances of these channels across a single stream of packets and some
convenient ways of constructing the channels and accessing them by message type.
This is what the `MessageChannels` interface provides.

### Questions you might ask

***Why would you ever need something like this?***

You would need this library only if most or all of the following is true:

1) You have a real time, networked game where TCP or TCP-like protocols are
   inappropriate, and something unreliable like UDP must be used for latency
   reasons.

2) You have a game that needs to send both fast unreliable data like position
   and also stream reliable game related data such as terrain data or chat or
   complex entity data that is bandwidth intensive.
   
3) You have several independent streams of reliable data and they need to not
   block each other or choke off fast unreliable data.

4) It is impractical or undesirable (or impossible) to use many different OS
   level networking sockets, or to use existing networking libraries that hook
   deeply into the OS or even just assume the existence of UDP sockets.

***Why do you need this library, doesn't XYZ protocol already do this*** (Where XYZ
is plain TCP, plain UDP, SCTP, QUIC, etc)

In a way, this library is equivalent to having multiple UDP connections and
bandwidth limited TCP connections at one time.  If you can already do exactly
that and that's acceptable for you, then you might consider just doing that
instead of using this library!

This library is also a bit similar to something like QUIC in that it gives you
multiple independent channels of data which do not block each other.  If QUIC
eventually supports truly unrleliable, unordered messages (AFAIK currently this
is only a proposed extension?), AND it has an implementation that you can use,
then certainly using QUIC would be a viable option.

***So this library contains a re-implementation of something like TCP, isn't
trying to implement something like that fiendishly complex and generally a bad
idea?***

Probably, but since it is designed for low-ish static bandwidth limits and
doesn't concern itself with congestion control, this cuts out a *lot* of the
complexity.  Still, this is the most complex part of this library, but it is
well tested and definitely at least works *in the environments I have run so
far*.  It's not very complicated, it could probably be described as "the
simplest TCP-like thing that you could reasonably write and use".

You should not be using the reliable streams in this library in the same way
that you use TCP.  A good example of what *shouldn't* probably go over this
library is something like streaming asset data, you should have a separate
channel for data that should be streamed as fast as possible and will always be
bandwidth rather than gameplay limited.

The reliable streams here are for things that are normally gameplay limited but
might be spikey, and where you *want* to limit the bandwidth so those spikes
don't slow down more important data or slow down other players.

***Why is this library so generic?  It's TOO generic, everything is based on
traits like `PacketPool` and `Runtime` and it's hard to use.  Why can't you just
use tokio / async-std?***

The `PacketPool` trait exists not only to allow for custom packet types but also
for things like the multiplexer, so it serves double duty.  `Runtime` exists
because I use this library in a web browser connecting to a remote server using
[webrtc-unreliable](https://github.com/kyren/webrtc-unreliable), and I have to
implement it manually on top of web APIs and it's currently not trivial to do.

### Current status / Future plans

I've used this library in a real project over the real internet, and it
definitely works.  I've also tested it in-game using link conditioners to
simulate various levels of packet loss and duplication and *as far as I can
tell* it works as advertised.

The library is usable currently, but the API should in no way be considered
stable, it still may see a lot of churn.

In the near future it might be useful to have other channel types that provide
in-between guarantees like only reliability guarantees but not in-order
guarantees or vice versa.

Eventually, I'd like the reliable channel to actually attempt congestion control
and avoidance.

The library desperately needs better examples, especially a fully worked example
using e.g. tokio and UDP, but setting up such an example is a large task by
itself.

## License

`turbulence` is licensed under either of:

* MIT license [LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT
* Creative Commons CC0 1.0 Universal Public Domain Dedication
  [LICENSE-CC0](LICENSE-CC0) or
  https://creativecommons.org/publicdomain/zero/1.0/

at your option.
