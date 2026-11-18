---
type: guide
title: Translation guide
description: Translation paths, metadata, pairing, and review rules for the bilingual corpus.
tags: [i18n, translation, zh-Hant]
status: stable
locale: en
translation_key: i18n-guide
---

# Translation guide

## Canonical paths

English is canonical. Translations live under `docs/i18n/<lang>/` and add the language suffix to the filename. The root `README.md` pairs with `docs/i18n/zh-Hant/README.zh-Hant.md`; the root `CONTRIBUTING.md` pairs with `docs/i18n/zh-Hant/contributing.zh-Hant.md`. The current supported language is `zh-Hant`.

## Metadata and pairing

Every translated topic and architecture document uses the seven keys `type`, `title`, `description`, `tags`, `status`, `locale`, and `translation_key`. Both README files omit metadata and retain corresponding heading structure. Other English and zh-Hant pairs keep the same translation key and corresponding heading structure.

## Review rules

Translate from the approved English owner without changing identifiers, paths, commands, links, warnings, or semantic strength. Internal links must resolve to existing documents. The root `SECURITY.md` uses a translation-key comment; its zh-Hant counterpart keeps YAML metadata.
