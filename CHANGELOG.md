# Changes

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
