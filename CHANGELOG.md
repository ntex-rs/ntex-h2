# Changes

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
