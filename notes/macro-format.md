# Macro share format

Human-readable text for one macro or a whole catalog of macros for a game.
Copy/paste between the start screen and a community page. The same grammar is
used for both.

## Single macro

```
game: 238960|Path of Exile
name: Hideout
action: focus > wait 100 > press enter > type "/hideout" > press enter
```

## Catalog (two or more macros)

```
game: 238960|Path of Exile
name: Hideout
action: focus > wait 100 > press enter > type "/hideout" > press enter
name: Remaining
action: focus > wait 100 > press enter > type "/remaining" > press enter
```

## Grammar

- `#` starts a comment (rest of the line). Blank lines are ignored.
- Lines before the first `name:` are **catalog headers**, `key: value`.
  - `game` is `<appid>|<name>`, split on the first `|`.
    - Appid: Steam id (digits). May be empty (`|Path of Exile`).
    - Name: display title for a standalone shortcut match. May be empty (`238960|`).
  - Any other header key is kept and written back on catalog export.
- `name: <label>` starts a macro. Names in one document must be unique and non-empty.
- Each macro has one `action:` line.
- Other `key: value` lines between `name:` and the next `name:` are that macro’s
  meta. They round-trip on copy.
- One `name:` block → import as a **single macro** (merge that name only).
- Two or more → import as a **catalog** (merge each name; leave local macros
  whose names are not in the paste alone).

## `action:` chain

Steps are separated by ` > ` (space, greater-than, space). The split happens
**outside** double quotes, so a `>` inside `type "..."` stays part of the text.

| Step | Meaning |
|------|---------|
| `focus` | Foreground the game this library entry is attached to |
| `wait <ms>` | Pause (each ≤ 2000; sum of waits in one macro ≤ 8000) |
| `press <key>` | Key down, then up |
| `type "..."` | Type the quoted string as Unicode (`\"` and `\\` escapes) |
| `keydown <key>` | Hold |
| `keyup <key>` | Release (any still-held keys are released when the macro ends) |

**Keys:** `enter`, `esc`, `tab`, `space`, `backspace`, `delete`, `up`, `down`,
`left`, `right`, `home`, `end`, `pageup`, `pagedown`, `f1`–`f12`, `a`–`z`,
`0`–`9`, with optional `ctrl+`, `alt+`, `shift+`, or `win+` prefixes
(`press ctrl+v`).

## Limits

- 16 KiB per imported document
- 24 macros per game
- 48 steps per macro

## Import binding (`game:`)

1. Non-empty appid matching a Steam catalog entry → that row (`steam:238960`).
2. Else a manual entry whose title equals the name (case-insensitive) → that row.
3. Several manuals share that title and no Steam match → refuse (ambiguous).
4. Nothing matches → import onto the game whose list is open; warn that `game:`
   did not match.

## Export

Writes `game: <appid>|<name>` from the catalog row (Steam appid + display name,
or `|` + shortcut title for a manual with no stored header).
