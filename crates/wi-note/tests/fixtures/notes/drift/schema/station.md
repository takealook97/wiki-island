---
title: Station Revision
type: schema
entity: Station
version: 1
schema:
  name: string, station label
  role?: string, purpose
  located_in?: Sector, assigned sector
  contact?: string, duty address
settings:
  validation: warn
---

# Early station card

A reduced metadata definition for usage-divergence parsing.
