# Changes

## [4.1.0] - Unreleased

* Add `StreamRef::is_reset()`, `StreamRef::on_reset()` and the same methods on `SendStream`,
  `on_reset()` returns an io waiter that completes when the stream is reset or the connection closes

* Document that only one task at a time may send payload or wait for send capacity on a stream,
  debug builds panic on concurrent capacity waiters

* Add `StreamRef::send_informational()`, sends `1xx` interim responses before the final response

* Add `Control::Expect` for requests with `Expect: 100-continue`, created by the application layer,
  `ControlAck::into_expect()` returns the request back with the result

* `StreamEof::Data` carries the receive-window `Capacity` of the final DATA frame, the connection
  window was released before the application consumed the data, `StreamEof` is not `Clone`

* Remove `StreamRef::poll_send_reset()` and `SendStream::poll_send_reset()`

* `SimpleClient::on_disconnect()` returns `ntex_io::Waiter<'static>`, `OnDisconnect` is removed from ntex-io

* Stream reset cancels all in-flight publish calls of the stream (HEADERS, DATA, trailers),
  only the HEADERS publish call was cancelled on reset

* Add `ServiceConfig::set_max_inflight_messages()`, limits in-flight service calls
  of a connection, default is 16,384

* Capacity timer task cancellation, e.g. on runtime shutdown, releases registered stream references

* DATA frame exceeding the stream receive window does not trigger a stream WINDOW_UPDATE
  before the stream reset

* A received GOAWAY honors `last_stream_id` (RFC 9113 §6.8): only locally initiated streams
  above it are failed, remaining streams complete, new streams are refused with
  `ConnectionError::GoAway`, the connection closes once the remaining streams are done

* Sending payload waits for the io write back-pressure release, a peer with large flow-control
  windows that does not read could make the sender buffer unbounded data

* Stream window updates do not restart the capacity timeout while the connection
  window is exhausted

* Waiting for send capacity fails with `OperationError::Closed` once the send side
  is closed, instead of waiting forever or reporting capacity

* Sending empty payload without eof does not wait for send capacity and does not
  send an empty `DATA` frame

* Remove unused `PseudoHeaders::request()`, `set_status()`, `set_scheme()`, `set_protocol()`
  and `set_authority()`, pseudo header fields are public

* Remove unreachable `Stream` re-export, unused `client::Observer` and `unstable` feature,
  `ClientBuilder::with_default()` and `SimpleClient::connection()`

* Fix build with the `trace` feature, API docs updates

* Close the connection with `SETTINGS_TIMEOUT` if the peer does not acknowledge local settings
  in time, new `ServiceConfig::set_settings_timeout()` (default 5 seconds) and
  `ConnectionError::SettingsTimeout`

* Server resets a request with the `:protocol` pseudo header with `PROTOCOL_ERROR`, extended
  CONNECT is not enabled

* Client validates a response to a `HEAD` request as a response without content, its
  `content-length` header was checked against the response `DATA`

* Client delivers interim `1xx` responses and waits for the final response, a following `HEADERS`
  was rejected as trailers without end of stream, an interim response with `END_STREAM` or `101`
  status is malformed (new `StreamError::InvalidInformational`)

* Client ignores late `HEADERS` for its closed streams without updating the last peer stream id,
  `HEADERS` for an idle client stream is a connection error

* Late frames for closed streams are not connection errors, `WINDOW_UPDATE` is ignored, `DATA`
  is reset with `STREAM_CLOSED`, trailers for a reset stream are ignored, frames for idle
  streams are still connection errors

* A publish call of a remote stream is cancelled only if the stream is reset during the call,
  request data after a complete response and the final message of a reset stream are published

* Document that a remote stream reset outside of a publish call does not get the final message

* Stream `WINDOW_UPDATE` is not sent for streams with a closed receive side, consumed data
  still releases the connection window

* Client does not fail a complete response on `RST_STREAM(NO_ERROR)`, the reset only stops
  the request body and is not counted as a reset

* Released stream slots wake a waiting request per free slot, a dropped woken request passes
  the wake up to the next waiting request

* Requests waiting for a stream slot fail on disconnect, keep-alive and read timeouts and
  graceful disconnect instead of waiting forever

* Capacity timeout resets the stream with `CANCEL` and does not count toward the stream resets
  limit. Default capacity timeout is 5 seconds

* Connection receive window is released when received data is consumed, the window bounds
  unconsumed data of all streams. Default connection window size is 4 MiB

* Ignore RST_STREAM for unknown or forgotten streams instead of a connection error

* Stop the capacity timer when a stream closes or fails, the timer does not keep closed streams
  alive

* Dropping a pending `send_capacity()` future stops the capacity timer, the stream is not reset
  with `FLOW_CONTROL_ERROR` without a waiter

* Control service failure fails open streams and publishes `Disconnect` for them, pending
  handlers do not block the connection shutdown

* PRIORITY frame with an invalid length is a stream error `FRAME_SIZE_ERROR`

* Accept `CONNECT` requests without `:scheme` and `:path`, reject them if present. Client omits
  `:scheme` and `:path` for `CONNECT` requests

* HPACK encoder compacts dynamic table entries, entries do not pin application buffers

* HPACK decoder compacts dynamic table entries with `trimdown()`, entries do not pin read buffers

* Client treats `SETTINGS_ENABLE_PUSH=1` from the server as a connection error `PROTOCOL_ERROR`,
  add `ConnectionError::UnexpectedEnablePush`

* Requests, responses and trailers over the peer's `SETTINGS_MAX_HEADER_LIST_SIZE` fail with
  `OperationError::HeaderListTooLarge`. `send_trailers()` returns `Result`

* Cap the HPACK encoder table at 4096 bytes, the peer's larger `SETTINGS_HEADER_TABLE_SIZE`
  is not used

* Use RFC 9113 error codes for frame decoding errors (`FRAME_SIZE_ERROR`, `FLOW_CONTROL_ERROR`,
  `COMPRESSION_ERROR`), GOAWAY debug data contains the decoding error. Add `FrameError::reason()`
  and `FrameError::InvalidInitialWindowSize`, fix swapped SETTINGS payload length errors

* A response without `:status` or with request pseudo-headers is a stream error with
  `PROTOCOL_ERROR`. Remove unused `ConnectionError::MissingPseudo` and
  `ConnectionError::UnexpectedPseudo`

* Publish stream reset message for locally closed streams

* `SETTINGS_INITIAL_WINDOW_SIZE` that overflows a stream send window is a connection
  `FLOW_CONTROL_ERROR`, instead of a stream reset

* Send the SETTINGS ACK after the peer's settings are applied

* The first frame from the peer must be SETTINGS, otherwise the connection is closed
  with `PROTOCOL_ERROR`. Add `ConnectionError::MissingSettings`

* Streams over the concurrency limit are always refused with `REFUSED_STREAM` and count
  toward the rapid reset limit, instead of closing the connection on the second overflow.
  Remove `ConnectionError::ConcurrencyOverflow`

* During shutdown all new streams are refused, DATA for refused streams is ignored

* Client assumes a limit of 100 concurrent streams until the peer's SETTINGS arrive,
  instead of no limit

* Wake streams waiting for send capacity when `SETTINGS_INITIAL_WINDOW_SIZE` grows the
  stream windows, senders stalled until the capacity timeout

* Add `ClientBuilder::connect_timeout()` and `ClientBuilder::disconnect_timeout()`

* Remove unused `control::Terminated`

* Document all public items and fix inaccurate API docs

* Reset only the stream on requests with missing or unexpected pseudo headers, instead of
  closing the connection, add `StreamError::MissingPseudo` and `StreamError::UnexpectedPseudo`

* Remove unused `FrameContinuationError::Malformed`

* Reset only the stream, instead of closing the connection, on malformed, too large or
  self-dependent header blocks and self-dependent PRIORITY frames, add `Frame::Invalid`,
  `frame::InvalidFrame` and `StreamError::InvalidFrame`

* Ignore DATA frames received on streams that were reset locally, instead of replying
  with `STREAM_CLOSED` resets

* Enforce the header list size limit on decoded headers, `Headers::load_hpack()` accepts `max_list_size`

* Count every header field toward `max_headers`, not only distinct names

* Fix quadratic copying when joining CONTINUATION frames

* Replace the generic length delimited codec with a dedicated HTTP/2 frame decoder

* Do not copy GO_AWAY debug data on decode, `GoAway::load()` accepts `Bytes`

* Reject DATA frames that exceed the receive window, with a `FLOW_CONTROL_ERROR` stream
  reset or connection error, add `StreamError::RecvWindowExceeded` and
  `ConnectionError::RecvWindowExceeded`

* `Codec::set_send_frame_size()` panics unless the size is between 16,384 and 16,777,215,
  a zero size made the encoder loop forever

* Count DATA frame padding toward flow control, add `frame::Data::flow_controlled_len()`

* Update to ntex-codec 2.0

* Export `client::ClientDisconnect`, the future returned by `SimpleClient::disconnect()`

* Wake a pending `RecvStream::recv()` when `SendStream` resets the stream, is dropped
  unfinished, or fails to send

* Apply the capacity timeout to `send_capacity()` and `poll_send_capacity()`, not only to
  payload sending, a stale capacity timer of a closed stream is ignored

* Client `SendStream` is not cancelled when `RecvStream` is dropped, the request body
  can be sent after the response is received

* Graceful disconnect waits for outstanding stream reservations, a reserved stream
  can be opened during graceful disconnect

* Add `SimpleClient::reserve()` and `StreamReservation`, a stream counted as active until
  the reservation is dropped or its request's stream is closed

* Add `SimpleClient::on_capacity()`, a callback called when a client stream is released,
  the peer changes its concurrent stream limit, or the connection is closed

* Fix `SimpleClient::active_streams()` returning `0` until the peer sends `MAX_CONCURRENT_STREAMS`

* Rename `ClientBuilder::maxconn()` to `ClientBuilder::connection_limit()`, to match the ntex http client pool configuration

## [4.0.1] - 2026-09-18

* Update api docs

## [4.0.0] - 2026-09-14

* Upgrade to ntex-service 5

* Service is used for Connector type

* http2 service use Service as handler type

## [3.13.0] - 2026-07-25

* Add support for connection lifetime

* Fix connection flow control for canceled streams

## [3.12.1] - 2026-07-13

* Avoid pending reset time underflow #96

## [3.12.0] - 2026-06-08

* Add "capacity" availability timeout

* Add "max headers" check, default is 96 headers

## [3.11.1] - 2026-05-05

* Add SendStream::send_pages() method

## [3.11.0] - 2026-05-05

* Add StreamRef::send_pages() method

## [3.10.0] - 2026-05-05

* Use new codec api with BytePages support

## [3.9.1] - 2026-04-07

* Cleanups

## [3.9.0] - 2026-04-02

* Update to ntex-error 2.0

## [3.8.1] - 2026-03-08

* Update ntex-error

## [3.8.1] - 2026-03-08

* Service name for simple client

## [3.8.0] - 2026-03-07

* Use ntex-error::Error

## [3.7.1] - 2026-02-16

* ServiceConfig is not Clone

## [3.7.0] - 2026-02-16

* SharedCfg is not Copy

## [3.6.2] - 2026-02-11

* Better clippy configuration

## [3.6.1] - 2026-02-02

* Make control module public

## [3.6.0] - 2026-02-02

* Refactor control messages

## [3.5.0] - 2026-01-29

* Use ntex_dispatcher::Dispatcher instead of ntex-io

## [3.4.0] - 2026-01-17

* Update bytes and codec apis

## [3.3.0] - 2026-01-04

* Use Bytes::advance_to() instead of .split_to()

## [3.2.0] - 2025-12-17

* Upgrade to ntex-service v4

## [3.1.0] - 2025-12-04

* Refactor stream ID validation and error handling #71

* Naming for ServiceConfig and ClientBuilder

## [3.0.0-pre.1] - 2025-11-30

* Set default max concurrent streams

* Fix spec compliance issues

## [3.0.0-pre.0] - 2025-11-27

* Update MSRV 1.85

* Use shared configuration

## [1.14.2] - 2025-11-12

* Update MSRV 1.82

## [1.14.1] - 2025-11-12

* Resolve keep-alive handling for idle state

## [1.14.0] - 2025-11-06

* Fix idle pings handling

## [1.13.0] - 2025-09-10

* Use ahash instead of fxhash

## [1.12.0] - 2025-07-08

* Use new errors from ntex-http

## [1.11.0] - 2025-07-01

* Allow to skip frames for unknown streams

## [1.10.0] - 2025-06-29

* Generate unique id for simple client

## [1.9.0] - 2025-06-13

* Update nanorand 0.8

## [1.8.6] - 2025-03-12

* Simplify delay reset queue

## [1.8.5] - 2025-02-12

* Fix handle for REFUSED_STREAM reset

## [1.8.4] - 2025-02-10

* Refactor delay reset queue

## [1.8.3] - 2025-02-09

* Re-calculate delay reset queue

## [1.8.2] - 2025-02-08

* Better handling for delay reset queue
* Fix connection level window size handling

## [1.8.1] - 2025-01-31

* Fix connection level error check

## [1.8.0] - 2025-01-31

* Add Client::client() method, returns available client from the pool

## [1.7.0] - 2025-01-30

* Add disconnect on drop request for client

## [1.6.1] - 2025-01-14

* Expose client internal connection object

## [1.6.0] - 2025-01-13

* Expose client internal information

## [1.5.0] - 2024-12-04

* Use updated Service trait

## [1.4.1/2] - 2024-11-07

* Fix type recursion limit

## [1.4.0] - 2024-11-04

* Use updated Service trait

* Better rediness error handling

## [1.3.0] - 2024-10-26

* Do not close connection if headers received for closed stream

## [1.2.0] - 2024-10-16

* Better error handling

## [1.1.0] - 2024-08-12

* Server graceful shutdown support

## [1.0.0] - 2024-05-28

* Use async fn for Service::ready() and Service::shutdown()

## [0.5.5] - 2024-05-01

* Fix ping timeouts handling

## [0.5.4] - 2024-04-23

* Fix Config::frame_read_rate() method

## [0.5.3] - 2024-04-23

* Add frame read rate support

* Limit "max headers size" to 48kb

* Limit number of continuation frames

* Do not decode partial headers frame

* Optimize headers encoding

## [0.5.2] - 2024-03-24

* Use ntex-net

## [0.5.1] - 2024-03-12

* Rename `ControlMessage` to `Control`

## [0.5.0] - 2024-01-09

* Release

## [0.5.0-b.0] - 2024-01-07

* Use "async fn" in trait for Service definition

## [0.4.4] - 2023-11-11

* Update ntex-io

## [0.4.3] - 2023-10-16

* Drop connection if client overflows concurrent streams number multiple times

* Drop connection number of resets more than 50% of total requests

## [0.4.2] - 2023-10-09

* Add client streams helper methods

## [0.4.1] - 2023-10-09

* Refactor Message type, remove MessageKind::Empty

* Refactor client pool limits

## [0.4.0] - 2023-10-03

* Refactor client api

* Add client connection pool

## [0.3.3] - 2023-08-10

* Update ntex deps

## [0.3.2] - 2023-06-23

* Fix client connector lifetime constraint

## [0.3.1] - 2023-06-23

* Refactor dispatcher, do not wrap services to a Pipelines

## [0.3.0] - 2023-06-22

* Release v0.3.0

## [0.3.0-beta.2] - 2023-06-21

* Fix handling stream capacity

* use ContainerCall instead of ServiceCall

## [0.3.0-beta.1] - 2023-06-19

* Use ServiceCtx instead of Ctx

## [0.3.0-beta.0] - 2023-06-16

* Migrate to ntex-service 1.2

## [0.2.5] - 2023-06-15

* Fix receive header handling for local streams

## [0.2.4] - 2023-05-11

* Expose connection stats

## [0.2.3] - 2023-04-12

* Better connection error info

## [0.2.2] - 2023-04-11

* Handle RST_STREAM, WINDOW_UPDATE, DATA for closed streams

## [0.2.1] - 2023-01-23

* Do not wait for capacity if it is availabe

## [0.2.0] - 2023-01-04

* Release

## [0.2.0-beta.0] - 2022-12-28

* Migrate to ntex-service 1.0

## [0.1.6] - 2022-12-02

* Fix KeepAlive handling

## [0.1.5] - 2022-11-10

* Drop server handler future if stream get reset

## [0.1.4] - 2022-07-13

* Disconnect client connection on client drop

## [0.1.3] - 2022-07-12

* Call publish service on connection error

## [0.1.2] - 2022-07-11

* Fix header_table_size setting handling #128

## [0.1.1] - 2022-07-07

* Allow to set client scheme and authority

## [0.1.0] - 2022-06-27

* Initial release
