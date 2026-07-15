//! Short, memorable level aliases for the workspace commands (`init`/`play`/`restart`).
//!
//! The canonical level slug — `stage-1-01-lru-eviction-debug` — is what the web
//! app's kit route (`07`) expects, but it's a mouthful to retype for every
//! attempt. This module lets a player name a level three shorter ways and
//! resolves them all back to that canonical slug:
//!
//!   * a one-word keyword    — `lru`, `crdt`, `schnorr`   (the primary form)
//!   * the level number 1-20 — `1`, `7`, `20`             (accepts `01`-padding)
//!   * a unique slug prefix  — `stage-1-01`, `stage-3-13`
//!
//! The keyword also names the workspace folder `init`/`play` create
//! ([`workspace_dir_name`]): `promptly play lru` unpacks into `./lru`, so the
//! `cd` after it is as short as the name the player just typed.
//!
//! Resolution is a pure, offline lookup against the frozen 20-level catalog, so a
//! short name never adds a network round-trip before `init` can fetch. Anything
//! that matches no known level is returned unchanged, so a full slug (or a
//! genuinely unknown one) flows through to the server exactly as before — the
//! feature only ever *adds* accepted spellings, it never rejects one.

/// One catalog entry: the canonical slug the server knows, and the short keyword
/// alias we also accept for it.
struct Level {
    slug: &'static str,
    alias: &'static str,
}

/// The frozen 20-level catalog, in (stage, level) order, so the list index + 1 is
/// the global level number a bare-number alias resolves by.
///
/// This mirrors `lib/levels/catalog.ts` in the web repo. The slugs are shipped
/// content, frozen at build time, so a static copy here can't drift in practice;
/// and the pass-through fallback in [`resolve`] means even a hypothetical drift
/// degrades to "type the full slug", never to a broken command. The keyword
/// aliases are the most salient token of each level, kept <= 10 chars, unique,
/// and free of any pure-number spelling so a number can never shadow a keyword
/// (all guaranteed by `catalog_is_well_formed` below).
const CATALOG: &[Level] = &[
    Level {
        slug: "stage-1-01-lru-eviction-debug",
        alias: "lru",
    },
    Level {
        slug: "stage-1-02-bounded-ring-buffer",
        alias: "ring",
    },
    Level {
        slug: "stage-1-03-sliding-window-rate-limiter",
        alias: "ratelimit",
    },
    Level {
        slug: "stage-1-04-crawler-race-fix",
        alias: "crawler",
    },
    Level {
        slug: "stage-1-05-orm-n-plus-1-fix",
        alias: "nplus1",
    },
    Level {
        slug: "stage-2-06-leader-election-debug",
        alias: "leader",
    },
    Level {
        slug: "stage-2-07-vector-clock",
        alias: "vclock",
    },
    Level {
        slug: "stage-2-08-crdt-convergence-debug",
        alias: "crdt",
    },
    Level {
        slug: "stage-2-09-worker-pool-deadlock",
        alias: "pool",
    },
    Level {
        slug: "stage-2-10-goroutine-conn-leak",
        alias: "connleak",
    },
    Level {
        slug: "stage-3-11-aead-timing-leak-debug",
        alias: "aead",
    },
    Level {
        slug: "stage-3-12-constant-product-amm",
        alias: "amm",
    },
    Level {
        slug: "stage-3-13-schnorr-merkle-verify",
        alias: "schnorr",
    },
    Level {
        slug: "stage-3-14-sqli-proto-pollution",
        alias: "sqli",
    },
    Level {
        slug: "stage-3-15-obfuscated-loop-debug",
        alias: "obfloop",
    },
    Level {
        slug: "stage-4-16-online-softmax-block",
        alias: "softmax",
    },
    Level {
        slug: "stage-4-17-bytecode-constant-fold",
        alias: "constfold",
    },
    Level {
        slug: "stage-4-18-sweep-and-prune-collision",
        alias: "sweep",
    },
    Level {
        slug: "stage-4-19-numpy-training-nan",
        alias: "nan",
    },
    Level {
        slug: "stage-4-20-scheduler-deadlock",
        alias: "sched",
    },
];

/// Resolve a player-typed level name to its canonical slug.
///
/// Tries, in order: an exact canonical slug, a keyword alias, the level number
/// (1-20), then a unique slug prefix. The first that matches wins. Input matching
/// nothing is returned unchanged (trimmed), so the caller's existing behaviour —
/// and the server's own "unknown level" error — is preserved. Matching is
/// case-insensitive; the returned slug is always the canonical lowercase form.
pub fn resolve(input: &str) -> String {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();

    // 1. Already a canonical slug — the common case for scripts and copy-paste.
    if let Some(level) = CATALOG.iter().find(|l| l.slug == lower) {
        return level.slug.to_string();
    }
    // 2. Keyword alias (`lru`, `schnorr`, …).
    if let Some(level) = CATALOG.iter().find(|l| l.alias == lower) {
        return level.slug.to_string();
    }
    // 3. Level number 1-20 (a zero-padded `01` parses the same as `1`).
    if let Ok(n) = lower.parse::<usize>() {
        if (1..=CATALOG.len()).contains(&n) {
            return CATALOG[n - 1].slug.to_string();
        }
    }
    // 4. A unique slug prefix (`stage-1-01`) — resolve only when it names exactly
    //    one level, so an ambiguous stem like `stage-2` falls through untouched
    //    rather than silently picking one.
    let mut prefixed = CATALOG.iter().filter(|l| l.slug.starts_with(&lower));
    if let Some(first) = prefixed.next() {
        if prefixed.next().is_none() {
            return first.slug.to_string();
        }
    }

    // No match — hand the original (trimmed) input back untouched.
    trimmed.to_string()
}

/// The directory name a level's workspace is created under by `init`/`play`: the
/// short keyword alias (`lru`), so the folder is as easy to `cd` into as the level
/// was to name — whichever accepted form (keyword, number, prefix, full slug) the
/// player actually typed. A slug outside the catalog falls back to itself,
/// mirroring [`resolve`]'s pass-through rule; the web app mirrors this mapping
/// when it renders `cd` copy, so keep the two in step.
pub fn workspace_dir_name(slug: &str) -> String {
    CATALOG
        .iter()
        .find(|l| l.slug == slug)
        .map(|l| l.alias.to_string())
        .unwrap_or_else(|| slug.to_string())
}

/// A one-line, copy-pasteable sample of the accepted short forms, for help/hint
/// text. Deliberately tiny — the full mapping lives in the web catalog.
pub fn example_forms() -> &'static str {
    "e.g. `lru`, `7`, or `stage-1-01`"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_keyword_alias_case_insensitively() {
        assert_eq!(resolve("lru"), "stage-1-01-lru-eviction-debug");
        assert_eq!(resolve("SCHNORR"), "stage-3-13-schnorr-merkle-verify");
        assert_eq!(resolve("  crdt  "), "stage-2-08-crdt-convergence-debug");
    }

    #[test]
    fn resolves_the_level_number_including_zero_padding() {
        assert_eq!(resolve("1"), "stage-1-01-lru-eviction-debug");
        assert_eq!(resolve("6"), "stage-2-06-leader-election-debug");
        assert_eq!(resolve("06"), "stage-2-06-leader-election-debug");
        assert_eq!(resolve("20"), "stage-4-20-scheduler-deadlock");
    }

    #[test]
    fn a_number_out_of_range_is_left_alone() {
        assert_eq!(resolve("0"), "0");
        assert_eq!(resolve("21"), "21");
        assert_eq!(resolve("999"), "999");
    }

    #[test]
    fn resolves_a_unique_slug_prefix_but_not_an_ambiguous_one() {
        // `stage-3-13` names exactly one level.
        assert_eq!(resolve("stage-3-13"), "stage-3-13-schnorr-merkle-verify");
        // `stage-2` names five — leave it for the server to reject rather than
        // guess which one the player meant.
        assert_eq!(resolve("stage-2"), "stage-2");
    }

    #[test]
    fn a_full_canonical_slug_resolves_to_itself() {
        for level in CATALOG {
            assert_eq!(resolve(level.slug), level.slug);
        }
    }

    #[test]
    fn an_unknown_name_passes_through_unchanged() {
        assert_eq!(resolve("totally-not-a-level"), "totally-not-a-level");
        assert_eq!(resolve(""), "");
    }

    #[test]
    fn the_workspace_dir_is_the_alias_for_every_catalog_level() {
        for level in CATALOG {
            assert_eq!(
                workspace_dir_name(level.slug),
                level.alias,
                "slug {}",
                level.slug
            );
        }
    }

    #[test]
    fn the_workspace_dir_for_an_unknown_slug_is_the_slug_itself() {
        assert_eq!(
            workspace_dir_name("totally-not-a-level"),
            "totally-not-a-level"
        );
    }

    #[test]
    fn every_keyword_and_number_round_trips_to_a_distinct_level() {
        for (i, level) in CATALOG.iter().enumerate() {
            assert_eq!(resolve(level.alias), level.slug, "alias {}", level.alias);
            assert_eq!(
                resolve(&(i + 1).to_string()),
                level.slug,
                "number {}",
                i + 1
            );
        }
    }

    /// Guards the invariants the resolver and its docs rely on, so a future edit to
    /// the table can't quietly break alias resolution.
    #[test]
    fn catalog_is_well_formed() {
        use std::collections::HashSet;

        assert_eq!(CATALOG.len(), 20, "the catalog ships exactly 20 levels");

        let mut slugs = HashSet::new();
        let mut aliases = HashSet::new();
        for (i, level) in CATALOG.iter().enumerate() {
            // Unique slugs and aliases — a duplicate would make resolution
            // order-dependent.
            assert!(slugs.insert(level.slug), "duplicate slug {}", level.slug);
            assert!(
                aliases.insert(level.alias),
                "duplicate alias {}",
                level.alias
            );

            // Aliases stay short, lowercase, and never look like a number (so the
            // numeric form can't shadow a keyword, and vice-versa).
            assert!(
                level.alias.len() <= 10 && !level.alias.is_empty(),
                "alias {} must be 1-10 chars",
                level.alias
            );
            assert!(
                level
                    .alias
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "alias {} must be lowercase alphanumeric",
                level.alias
            );
            assert!(
                level.alias.parse::<usize>().is_err(),
                "alias {} must not be a bare number",
                level.alias
            );

            // The list index fixes the global level number, which must be the
            // zero-padded `NN` embedded in the slug (`stage-S-NN-…`).
            let nn = format!("-{:02}-", i + 1);
            assert!(
                level.slug.contains(&nn),
                "slug {} must carry its level number {}",
                level.slug,
                nn
            );
        }
    }
}
