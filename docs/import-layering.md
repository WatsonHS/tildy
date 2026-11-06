# Starship import and project display layers

```sh
tildy import starship --file ~/.config/starship.toml --json
```

Inspect `candidate` and `report.ignored`, `report.bridged`, `report.manual` before applying. `--write` uses the normal configuration transaction and refuses to overwrite an existing user file. Custom commands require manual migration. The importer is not a guarantee of full Starship equivalence.

Bridged modules require Starship and retain the source TOML's absolute path. Missing or unreadable sources produce a module error; they do not silently switch to another configuration. Review paths when moving configuration between machines.

The nearest Git root can contain `.tildy.json`:

```json
{
  "schema_version": 1,
  "modules": {
    "cwd": { "style": { "foreground": "cyan", "bold": true } }
  }
}
```

`cwd` must already exist in the lower configuration. The layer can alter display and layout, but cannot define commands, change paths or activate a module absent from the lower layout. Invalid files are ignored as a whole layer. Set `TILDY_PROJECT_CONFIG=0` before shell initialization to disable it.

```sh
tildy config get --effective --cwd /path/to/repo --json
tildy config validate --project --file .tildy.json --json
tildy schema --project --json
```

`revision` identifies the user file for transactions; `effective_revision` combines valid runtime layers. Project errors and provenance are reported separately. See [ADR 0012](decisions/0012-repository-display-layer.md).
