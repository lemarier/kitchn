# House template fixtures

These directories are house-owned example content used only by the scaffold
tests. They are not Kitchen defaults and are never installed by Kitchen.

- `origin89/rust-workspace`: the Origin89 house's repository template, derived
  from this repository's layout and the Origin89 engineering conventions. The
  drift test in `tests/scaffold.rs` compares it with this repository.
- `crabnebula/tauri-app`: a synthetic second house with its own Tauri and specta
  conventions. It proves that one house's rules and names do not leak into
  another house's repository. It is not CrabNebula's real policy.

The Origin89 fixture omits the upstream `.origin89/` bootstrap assets. Before
using its `skills-sync` recipe, supply the script and all three notices/licenses
listed in [the template guide](../../../../../templates/README.md#house-guidance-and-bootstrap-assets)
from a reviewed export. Offline `just check` does not validate guidance loading.
