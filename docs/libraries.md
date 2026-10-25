# Libraries

A library is a folder of reusable motion: compositions, animation presets, footage and sound
effects, indexed by a `library.json` in the folder. People and agents search it and place what
they find in the open composition. After Effects' closest feature is Creative Cloud Libraries;
EffectCraft's libraries are plain local folders, so a team can sync them however it likes.

## Commands

All of them are engine commands, so the menus, the CLI, the control channel and MCP
(`command_run`, `command_list`, `describe_command`) use them alike.

| Command | Does |
|---|---|
| `library.addFolder {path}` | Adds a folder that holds a `library.json` (kept in the settings as `libraries`) |
| `library.removeFolder {path}` | Removes it |
| `library.folders` | Lists the folders with their entry counts, or why one can't be read |
| `library.search {query?, kind?, category?, limit?, offset?}` | Finds entries. Every word of `query` must match the name, tags, category or description; name matches rank first, then tags, category and description; whole words and prefixes beat substrings. No query lists everything by name |
| `library.get {id}` | One entry with its file, composition, preview and folder |
| `library.insert {id, comp?, time?, layers?}` | Places an entry (below) |

`library.insert` by kind:

- `comp`: imports the `.ecproj` with `file.importProject` (into a folder named after the file) and
  nests the entry's `comp` (by name; else the project's only root comp) in `comp` or the open
  composition, at `time` or the current time. Two undo steps: the layer, then the import.
- `footage`, `audio`: imports the file and adds it as a layer.
- `preset`: applies the `.ecpreset` to `layers` or the selected layers.

With no composition open, comps and media are only imported. An entry whose file is missing is an
error that changes nothing.

## Reading an entry (agents)

An agent that should *reuse* a `comp` entry rather than drop it in as is reads it as a recipe:
`library.get {id}` gives the `.ecproj` and comp name; `file.importProject {path}` brings it in;
`comp.recipe {comp}` returns only what was authored (layers, shape contents, effects, styles,
changed values and keyframes, a few KB instead of the project's tens), and
`comp.fromRecipe {recipe}` builds an adapted copy after the agent edits it. See
[agents.md, Reuse a library comp](agents.md#reuse-a-library-comp-recipes).

## `library.json`

```json
{
  "format": "effectcraft-library",
  "version": 1,
  "name": "Studio",
  "entries": [
    {
      "id": "lower-third",
      "name": "Lower Third",
      "kind": "comp",
      "file": "projects/lower-third.ecproj",
      "comp": "Lower Third",
      "category": "Titles",
      "tags": ["name", "caption"],
      "description": "Name and role slide in from the left",
      "duration": 4.0,
      "thumbnail": "thumbs/lower-third.png",
      "preview": "previews/lower-third.mp4"
    }
  ]
}
```

`kind` is `comp`, `preset`, `footage` or `audio`. Relative paths resolve against the library
folder. Entries without an `id` are skipped, and fields EffectCraft doesn't know are ignored, so
other tools can keep their own data next to these fields.

## Bringing an After Effects library over

Convert each `.aep` to an `.ecproj` with `packaging/aep/convert_aep.py` (see
[packaging/aep/README.md](../packaging/aep/README.md)), keep the `.aep` files as the originals, and
list the converted projects in `library.json`. Convert again when the AEP importer improves: the
`.ecproj` is a derived copy, not the original.
