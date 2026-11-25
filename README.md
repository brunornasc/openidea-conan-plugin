# conan — Conan Center in the IDE

**Tools → Conan: Search Conan Center** (or the Conan tool window) opens a
search over the recipes Conan Center publishes. A search asks center2's JSON
API for every reference whose name matches and lists each recipe's versions,
newest first; picking one shows what conan.io's own search API says of it
(description, homepage, licences and labels).

**Install**, **Update**, **Downgrade** and **Remove** change the project's
`conanfile.txt`, run `conan install`, edit `CMakeLists.txt` and configure CMake:

- The requirement replaces (or is removed from) `[requires]`; a `[settings]`
  section is dropped, because Conan 2 takes settings from the profile, and
  `CMakeDeps`/`CMakeToolchain` are added to `[generators]`.
- `conan install . --output-folder <build>/conan --build=missing -s build_type=…`
  runs in the project root **once per build type, Release and Debug**, so
  `CMakeDeps` writes the data files a Debug configure needs (without the Debug
  install, `find_package()` finds no include dirs and the Debug build fails).
  If Conan says the profile is missing, the plugin runs
  `conan profile detect --force` once and retries. The processes run behind a
  cancellable background task, so a long install is not cut off.
- `find_package()` and the `name::name` link targets come from the
  `CMakeDeps necessary find_package() and targets` block in that output, so the
  names are Conan's, not the recipe's. They are added before the first
  `add_executable()` and to its `target_link_libraries()`, without duplicating
  what is already there. Removing a recipe also removes its `name::` targets.
- CMake is configured with the generated toolchain and prefix path
  (`-DCMAKE_TOOLCHAIN_FILE=<build>/conan/conan_toolchain.cmake`,
  `-DCMAKE_PREFIX_PATH=<build>/conan`, `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`)
  and `-DCMAKE_BUILD_TYPE` taken from the folder's `CMakeCache.txt`, or `Debug`
  for a new project — no longer forced to Release, so a Debug build keeps its
  symbols and GDB can bind breakpoints. The build folder is one the project
  already has (`build`,
  `cmake-build-debug`, `cmake-build-release`, `cmake-build-relwithdebinfo`,
  `out/build` with a `CMakeCache.txt`), else `build`. A cache built without
  Conan's toolchain is reconfigured with `--fresh`; an older CMake retries
  without it.

The list's `installed` badges and the project's packages (in an empty search)
come from `conanfile.txt`, or from a `conanfile.py`'s quoted requirements. A `conanfile.py` is read but never edited: its packages
are listed and changes are refused with a message saying so.

The page is HTML (`Body::Html`) talking to the plugin with
`openidea.postMessage`/`onMessage`; there is no web build. Settings are the
project's own except that the plugin never writes `conanfile.py`.

## Language

The plugin's text is in English by default and follows the IDE's language when
the IDE is set to Portuguese (Brazil). A **Plugin language** setting can pin it
to `auto`, `en-US` or `pt-BR`. The translation lives in `locales/pt-BR.json`: the
IDE translates the manifest and view text, while the plugin uses the same
catalog for the text it builds itself (errors, notifications and the HTML page).

## Capabilities

`project.read`, `project.write`, `net:center2.conan.io`,
`net:center.conan.io`, `net:conan.io`, `process:conan`, `process:cmake`.

## Requirements

Conan 2 (`conan` on `PATH`) and CMake (`cmake`) for the install paths;
searching and browsing need neither. Discovery uses
`https://center2.conan.io/v2/conans/search` with
`https://center.conan.io/v2/conans/search` as the fallback and
`https://conan.io/api/search/<name>` for the details, never the HTML page.

## Build and test

```sh
openidea-plugin build
cargo test
openidea-plugin check
```

Install the built package folder through Settings › Plugins › *Install Plugin from Disk…*.
See the [OpenIDEA plugin guide](https://github.com/l0ng-ai/openidea/blob/main/plugins/README.md).

## Icon

`icon.svg` is the Conan logo from [Simple Icons](https://simpleicons.org/?q=conan)
(CC0), in Conan's brand blue (`#6699CB`). Conan and its logo are trademarks of
JFrog Ltd.; they identify the service this plugin talks to, not an endorsement.
