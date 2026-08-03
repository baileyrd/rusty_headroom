# Architecture

## Overview
<!-- What this system does, in a few sentences. What it's not (non-goals). -->

## Boundaries
<!-- Domain logic vs. I/O and framework details (ports-and-adapters).
     List the ports (interfaces) and the adapters that implement them. -->

No ports or adapters exist yet — there's no source in this repo. The table stays
empty rather than carrying invented rows; fill it in with the first real boundary.

| Port | Adapter(s) | Notes |
| ---- | ---------- | ----- |
|      |            |       |

## Structure
Modular monolith. Composition over inheritance. Ports-and-adapters keeps domain
logic free of I/O and framework details — the domain defines the interface, the
adapter implements it, and domain code never imports a backend directly.

A component gets extracted into its own service only for a concrete forcing
function: independent scaling, a team or language boundary, or hard fault
isolation. "It feels like a separate thing" is not one. This repo hasn't crossed
that line — there's nothing here yet to split.

## Data flow
<!-- Diagram or short walkthrough of a request/event through the system -->

## Key decisions
See [docs/adr/](./docs/adr/) for the record of individual decisions and their tradeoffs.

## Non-goals
<!-- Explicitly out of scope, and why -->
