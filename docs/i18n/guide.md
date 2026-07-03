# Translations (`docs/i18n/`)

This directory houses translated versions of ccync's documentation, organized with one subfolder per language. It serves simply as a working location rather than a strict policy engine—the canonical English documents remain in their original locations and are never moved. This specific file serves as an English-only signpost and should not be translated.

ccync's primary and canonical language is English (defined as `PROJECT_LANGUAGE = en` in `.dev/project.md`). Translations are entirely optional supplementary materials; the lack of a translation for a specific document is never considered a blocking issue.

## Directory Layout

```text
docs/i18n/
└── <lang>/                    e.g., zh-Hant, ja
    ├── README.<lang>.md        ← Mirrors the repository root's README.md
    ├── manual.<lang>.md        ← Mirrors docs/manual.md
    └── naming.<lang>.md        ← Mirrors docs/naming.md
```

A translated file must mirror the path of its canonical source and include the target language code in both its parent folder and its filename.

## Translatable Sources

Translations are restricted to user-facing documentation and convention guidelines. The current allowlist includes:

- `README.md` — The landing page and onboarding guide.
- `docs/manual.md` — The command surface documentation.
- `docs/naming.md` — Reserved terminology and runtime keys (our vocabulary authority).

Internal, developer-focused documents (such as `architecture.md`, `devguide.md`, `release.md`, and `contributing.md`) are maintained exclusively in English.

## Current Languages

As of now, there are no active translations. This folder simply establishes the convention; no translated content has been committed yet.

## Adding a Translation

To contribute a new translation, follow these steps:

1. Select a document from the translatable sources allowlist above.
2. Create a new file at the corresponding mirrored path: `docs/i18n/<lang>/<name>.<lang>.md`.
3. Begin the file with the following YAML freshness front-matter:

   ```yaml
   ---
   source: README.md          # The repository-relative path to the canonical English source
   lang: <lang>               # e.g., zh-Hant, ja
   source_commit: PENDING     # The commit hash of the English source this translation reflects
   translated_at: YYYY-MM-DD
   status: current
   ---
   ```

4. Translate the body of the document, ensuring that all headings and markdown anchors remain perfectly aligned with the original source.
5. Add a language-switcher link at the top of the canonical English document, and provide a reciprocal link back from the translated file.
6. Once you are ready to commit, replace `PENDING` in the `source_commit` field with the actual commit hash of the source file. You can find this by running: `git log -1 --format=%H -- <source>`.

## Maintaining Freshness

ccync does **not** provide automated tooling to track translation freshness. Staleness must be tracked manually: a translation is considered current if its `source_commit` matches the latest commit that modified the canonical source (`git log -1 --format=%H -- <source>`). If the hashes differ, the translation is stale and should be reviewed and updated.

## Adding a New Language

To introduce a new language, simply create a new `docs/i18n/<lang>/` directory and add your translations following the steps above. This convention effortlessly scales to support any number of languages without requiring tooling modifications.
