---
title: Station Definition
type: schema
entity: Station
version: 2
schema:
  name: string, station label
  role?: string, field function
  maintained_by?: Crew, upkeep group
  equipment?(array): string, installed instruments
  contact?: string, contact address
settings:
  validation: warn
---

# Inventory definition

- [purpose] Describe fictional survey records
- [convention] Labels match the storage register

- used_by [[Lantern Station]]
- used_by [[West Marker]]
