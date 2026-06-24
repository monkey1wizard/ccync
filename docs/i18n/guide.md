# Translations (`docs/i18n/`)

This folder holds translated copies of ccync's documentation, one subfolder per language. It is a
working location, not the policy — the canonical English docs stay in their normal location and are
never moved. This file is an EN-only signpost; do not translate it.

ccync's canonical language is English (`PROJECT_LANGUAGE = en`, see `.dev/project.md`). Translations
are optional and follow-up; an absent translation is never a blocker.

## Layout

```text
docs/i18n/
└── <lang>/                    e.g. zh-Hant, ja
    ├── README.<lang>.md        ← mirrors the repo-root README.md
    ├── manual.<lang>.md        ← mirrors docs/manual.md
    └── naming.<lang>.md        ← mirrors docs/naming.md
```

A translation mirrors its canonical source's path and carries the language in both the folder and
the filename.

## Translatable sources

The allowlist is the user- and convention-facing docs:

- `README.md` — landing + onboarding.
- `docs/manual.md` — the command surface.
- `docs/naming.md` — reserved terms + runtime keys (vocabulary authority).

Internal docs (`architecture.md`, `devguide.md`, `release.md`, `contributing.md`) are EN-only.

## Current languages

None yet. This folder ships the convention; no translation has been committed.

## Add a translation

1. Pick a source from the allowlist above.
2. Create `docs/i18n/<lang>/<name>.<lang>.md` at the mirrored path.
3. Start the file with freshness front-matter:

   ```yaml
   ---
   source: README.md          # repo-relative path to the canonical source
   lang: <lang>               # e.g. zh-Hant, ja
   source_commit: PENDING     # the source commit this translation matches
   translated_at: YYYY-MM-DD
   status: current
   ---
   ```

4. Translate the body, keeping headings and anchors aligned with the source.
5. Add a language-switch link at the top of the canonical doc (and back from the translation).
6. After committing, stamp `source_commit` with `git log -1 --format=%H -- <source>`.

## Freshness

ccync ships **no** translation-freshness command. Track staleness manually: a translation is
current when its `source_commit` equals the latest commit that touched its source
(`git log -1 --format=%H -- <source>`); otherwise it is stale and should be refreshed.

## Add a language

Create `docs/i18n/<lang>/` and add translations as above. The convention scales to any number of
languages with no tooling change.
