---
title: Publication Definition
type: schema
entity: Publication
version: 2
schema:
  label: string, catalog label
  editor?: string, preparing group
  revision?: integer, print revision
  shelf?(enum): [charts, manuals, logs]
settings:
  validation: warn
---

# Inventory definition

- [purpose] Describe fictional survey records
- [convention] Labels match the storage register
