# homelinter Design Specification

## Overview

homelinter organizes home directories: it scans files, analyzes them, generates reorganization suggestions, and executes those suggestions automatically or interactively.

One service owns scanning, analysis, planning, validation, and execution. The CLI, TUI, and SPA are clients of that service. The CLI and TUI launch an invocation-scoped service, while `homelinter web` launches a service that remains available until terminated.

**Non-goals**: homelinter is not transactional across operations, does not emulate host capabilities that are unavailable, and does not guess at semantics it cannot verify safely (see "Execution & Safety").

## Platform Scope

homelinter supports Linux, macOS, and Windows as first-class platforms. The native executor and filesystem safety checks are the reference implementation on every platform. Platform-specific behavior is selected at runtime and is never inferred from Unix command syntax alone.

Linux and macOS may export POSIX shell scripts. Windows supports drive-letter and UNC paths and exports PowerShell scripts. Exported scripts are platform-specific, are not portable between operating systems, and are always produced for the host platform.

Paths, path comparison, filename validation, file identity, timestamps, permissions, links, and metadata preservation use host-platform semantics. Serialized plans and audit records retain host-native absolute paths and include the platform identifier. A capability unavailable on the host is reported as unsupported or failed rather than emulated unsafely.

On Windows, homelinter distinguishes symbolic links, junctions, mount points, and other reparse points. Only link types explicitly supported by the implementation are indexed or repaired; unsupported reparse points are never followed or modified. Windows link operations require the permissions and operating-system features needed by the selected link type.

## Core Principles

- **Safety first**: homelinter never overwrites an existing object, modifies file contents, follows an unsupported link, or executes a stale or colliding operation.
- **Non-destructive by default**: scanning, analysis, and planning never modify the filesystem. A plan export writes a script file to the user-specified path but never modifies the scanned home tree; destructive operations on the scanned tree require explicit execution authorization.
- **Honest failure reporting**: operations are validated immediately before execution, and failures and partial completion are reported precisely without claiming rollback that did not occur.
- **Leverage existing tools**: prefer modern Rust tools (`fd`, etc.) over older counterparts (`find`), with graceful fallbacks.
- **Convention without explicit configuration**: infer sensible defaults from the user's existing directory structure, patterns, and installed tools in the absence of explicit configuration.
- **User in control**: all preferences are overridable via command-line arguments and configuration files.

## Scan Root and Path Representation

The user may instruct homelinter to scan any accessible directory with the CLI `--root <path>` option or the corresponding per-scan/per-session setting in an interactive client. The scan root is not persisted in the configuration file. If no scan root is specified, the scan root is `$HOME`.

The scan root defines the root of the primary discovery tree: homelinter discovers and analyzes files, directories, and repository units only at or below that root, subject to the hidden-file, ignore, and exclusion rules. It does not limit where a discovered object may be moved.

All filesystem paths in suggestions, exported plans, client APIs, and user-facing output are absolute paths in the host platform's native representation. This includes the scan root, `target`, and `new_path`. Paths are never interpreted relative to the process's current working directory, the scan root, or the platform's home directory. A destination may be outside the scan tree and outside the user's home directory, for example on a mounted USB device. Destination selection remains subject to configured permissions and execution safety checks.

- Input paths for `--root`, configuration, and target directories are resolved according to their documented input rules and converted to absolute paths before scanning or planning.
- Emitted paths must be normalized absolute paths using host-platform rules. They must not contain `~`, environment-variable references, redundant `.` components, or a relative-path prefix.
- Unix paths, Windows drive-letter paths, and Windows UNC paths are accepted according to the host platform; paths are not converted between platform syntaxes.
- Normalization must not resolve symlinks merely for display or serialization. The lexical absolute path and the filesystem identity it names must be preserved separately where necessary.
- The scan root is captured when the scan starts and is retained with the scan and execution plan. A later change to the process's current working directory or environment must not change the meaning of a suggestion.
- Before execution, `target` must still identify the discovered source object, and a `move` destination may be anywhere allowed by the plan. The destination is not required to be under the scan root or `$HOME`.

`target` and `new_path` in a Suggestion are absolute paths. Internal APIs use platform-native path types, while client and serialization boundaries emit absolute paths in the host platform's representation. Audit records use the same representation and record the host platform.

## Symlink Handling

Symlinks are not ordinary discovery units and receive a separate treatment from primary discovery.

**The home-wide symlink pass** is the deliberate exception to the scan-root boundary. For every scan, homelinter also performs a separate symlink pass over the entire user's home directory, regardless of `scan.root`. This pass finds symlinks that need repair when a discovered source is moved or deleted. It records symlink entries and their raw targets but does not follow symlinks or scan their target contents. The pass includes symlinks in hidden and VCS-ignored directories; `scan.root` does not restrict it, and `scan.exclude_paths` does not exclude symlinks from it, because doing so could leave references to moved or deleted objects broken.

The pass derives its scope from the platform's home environment variable resolved at scan start; the resolved home path is captured with the scan, so a later environment change does not change the repair-pass coverage. The pass collects only the metadata needed for repair - absolute link path, raw link target, link type, and the resolved target identity when resolvable - and never reads symlink target contents. This is a deliberate scope and privacy trade-off: a scan rooted below `$HOME` still reads symlink metadata across the entire home directory (including hidden and VCS-ignored directories) to keep moves and deletes from breaking links. Repairs are never performed for links unrelated to a planned move or delete, so a narrow scan gains no repairs outside its own suggestions.

**Primary scan**: the primary scan never follows symlinks. Directory symlinks are recorded but never traversed or analyzed as directories. The pass preserves each link's raw target and whether that target is relative or absolute, because repair must preserve the link's intended target while accounting for the link's parent directory.

**Repair**: during execution, symlinks in the home-directory index are eligible for repair only when their target is the source of a planned `move` or `delete`. A link to a moved file or directory is repaired to point to the new absolute target, represented in the link using the appropriate relative or absolute form. A link to a deleted duplicate is repaired to point to the retained original only when the duplicate relationship and target identity were confirmed.

Repair is always performed under safe-only semantics: only links whose recorded identity and target were confirmed at scan time are repaired, unsupported reparse points are never followed or modified, and a repair is validated again immediately before it runs. Repair applies only to the link's path target metadata and never changes file contents. It preserves whether the original link target was relative or absolute: relative replacement targets are recalculated relative to the symlink's parent directory, while absolute replacement targets remain absolute.

- For a moved directory, symlinks targeting the directory or any descendant are associated with the move and are repaired using the corresponding old-to-new path mapping.
- For a deleted duplicate, symlinks to the deleted object may be repaired to point to the retained original only after the delete operation's preconditions and duplicate identity are confirmed.
- Immediately before the parent operation, homelinter validates the symlink path, raw link target, and resolved target identity against the scan-time record. Immediately before repair, it validates the same state again and replaces the link without following it.

Repair is a dependent `repair_symlink` operation (see "Suggestion Model"); it exists only as a dependency and is never surfaced or executed independently.

## File Classification

homelinter categorizes files into **Asset Kinds**:

| Kind | Description | Example Extensions |
|------|-------------|-------------------|
| `document` | Text documents and office files | `pdf`, `docx`, `odt`, `txt`, `md`, `epub` |
| `photo` | Raster and vector images | `jpg`, `jpeg`, `png`, `gif`, `webp`, `svg`, `raw`, `cr2`, `nef` |
| `video` | Video files | `mp4`, `mkv`, `avi`, `mov`, `webm` |
| `audio` | Music and sound files | `mp3`, `flac`, `ogg`, `m4a`, `wav`, `aac` |
| `repository` | Version control repositories | `.git`, `.hg`, `.svn` (detected by a directory marker, or a `.git` file) |
| `archive` | Compressed files | `zip`, `tar`, `gz`, `bz2`, `7z`, `rar` |
| `data` | Structured data files | `json`, `xml`, `csv`, `yaml`, `sql`, `db` |
| `executable` | Binaries and scripts | `exe`, `app`, `sh`, `py`, `rb`, `js`, `wasm` |
| `other` | Everything else | - |

**Empty files**: files of zero bytes are completely ignored. They are not scanned, not classified, and no suggestions are generated for them. They are never duplicates regardless of hash.

**Hidden files**: whether hidden files and directories are scanned is a per-scan, per-session setting, resolved exactly like the scan root and never read from the configuration file. The CLI selects it with `--hidden`, and the TUI and SPA select it through per-scan/per-session controls. By default, when no override is given, hidden files and directories are excluded. When using `fd`, homelinter passes explicit flags (regardless of `fd`'s own defaults) to enforce this convention, consistent with `scan.exclude_paths` and the selected hidden setting. `find` is invoked with arguments that match the intended scope. The operation is deterministic from homelinter's configuration; tool defaults do not influence behavior.

**Hidden-file definition by platform**: what counts as hidden is platform-native. On Linux and macOS, a path component at any depth (excluding `.` and `..`) is hidden when its name begins with a dot; VCS ignore rules are never consulted, because repositories are atomic units whose contents are never descended into, and dotfile classification does not read ignore files. On Windows, the definition is configurable via `scan.hidden_definition` (see "Configuration File Schema"): `explorer` (default) - an object is hidden when it has the Hidden or System file attribute, the union Explorer and common search tools treat as not displayed by default; `attribute` - the Hidden attribute alone; `dotfiles` - names beginning with a dot, matching Unix semantics. The definition gates only primary discovery, just like the boolean `--hidden` itself, and never the home-wide symlink repair pass; it is applied deterministically from the configuration value, never from the shell's or Explorer's display settings.

**Repositories**: a directory is classified as `repository` if it contains a `.git` directory or `.git` file, a `.hg` directory, or a `.svn` directory. The `.git` file form covers git worktrees and other detached marker setups, whose marker is a regular file (a `gitdir:` pointer) rather than a directory. A repository is treated as an atomic unit: its contents are not scanned, classified, hashed, or analyzed, and the tool may suggest moving or renaming only the repository root, never restructuring internals. VCS ignore rules do not control repository discovery or primary discovery; repositories are detected structurally. Nested repositories (including submodules) are ignored for scanning and classification, but they remain physically inside the parent repository and move with it if the parent root is relocated.

**Symlinks**: see "Symlink Handling".

## Analysis Pipeline

For each discovered file or repository unit in the primary scan, homelinter collects:

1. **Filesystem metadata**: size, modification time, creation time, permissions, owner
2. **Kind-specific metadata**:
   - Photos/Videos: EXIF / IPTC / XMP data (date taken, camera, GPS, orientation)
   - GPS coordinates from photos and videos are reverse-geocoded to a locality by the configured geocoder when it is enabled, reduced to the city (or a finer neighbourhood for configured cities; see "Destination Path Format Strings")
   - Documents: title, author, and creation date from embedded container metadata; page count is available for PDFs only (see "Tooling Strategy")
   - Audio: ID3 tags, bitrate, duration
   - Repositories: remote URLs, default branch, commit count, last activity, collected by the per-VCS adapter (see "Tooling Strategy")

Provider-derived location (`%l`) and description (`%s`) values are resolved **asynchronously, after the scan**: the scan plans and enqueues provider requests before publishing, marks the suggestions that depend on them `metadata_pending`, publishes the full batch immediately, and a background worker fetches the queue with per-provider rate limits (see "Provider Plugins > Offline request loop"). **A provider is consulted only when the destination format for a unit's kind actually uses the corresponding placeholder**: a geocode request is planned only for a unit whose format contains `%l` and that carries raw GPS coordinates, and a description request only for a unit whose format contains `%s` with a description provider enabled for its kind, and a document-author request only for a document whose format contains `%a` and whose embedded metadata carries no author; format strings without the placeholder never trigger a provider call for that unit. A pending suggestion is visible (with a `[metadata pending]` marker in the TUI and web UI) but **not executable** until every provider request for its unit resolves, so a user can never execute a move built on incomplete metadata. When the last request of a unit resolves, the suggestion is updated in place by its stable id: a success rebuilds the move with the collected values, a failure keeps generated fallbacks, using the original filename stem without its extension for `%s` (and the next scan retries), and `metadata_pending` flips to `false`. The batch is then re-validated against the now-real metadata: a destination that only collides once the provider value is applied is rejected with a reason (broadcast as `batch.validated`), while a pending suggestion that would have lost a provisional collision stays pending. Clients receive an updated `suggestion.batch` on each update. A scan is never blocked on a provider request, and a new scan supersedes outstanding requests from the previous one via a per-session epoch.
3. **Content-derived metadata** *(optional, user-configurable)*:
   - An LLM analyzes document subject matter (e.g., "invoice", "travel photo", "tax document")
   - A description service produces a short description of the content, used by the `%s` placeholder. Descriptions are LLM-backed today through the provider settings in `[description]`; photo descriptions may also send the raw image bytes to an image-capable backend (`ollama` with a vision model) so the model analyzes the picture itself rather than guessing from the filename - and photos larger than `max_image_dimension` are downscaled before the request so the model sees fewer image tokens - while video and audio recognition providers remain future work behind a separate provider interface. A configurable default provider (the base `[description]` entry) may serve the content-modality kinds, with per-modality providers overriding it when needed; kinds without a content source never get a description provider, so `%s` falls back to the filename stem for them.
   - Documents are preprocessed to plain text before the description service runs, using the configured extraction tool for their format (see "Tooling Strategy"). Text extraction is disabled by default and incurs no cost when the description service is off.
   - The model and provider backend are configurable by the user (local via Ollama/LM Studio, or remote via OpenRouter; see "Provider Plugins")
   - LLM analysis and description services are **disabled by default** and incur explicit cost/privacy trade-offs
   - Reverse-geocoding and description results are stored in a persistent cache so repeated scans do not make redundant requests and continue to produce stable user-readable values.
4. **Filename- and path-derived metadata**: the filename and relevant parent-directory path may be analyzed by an optional LLM when higher-priority metadata and content-based analysis do not provide the requested value. This analysis reads only the name and path, never the file bytes. It is distinct from the `%f`, `%F`, `%n`, and `%e` placeholders, which use the filename directly. Filename-based LLM analysis is **disabled by default**, uses the same configurable-provider and persistent-cache machinery as content-derived metadata, and is the last metadata-analysis stage before placeholder-specific generic fallbacks.

For format-string metadata, the general precedence is: kind-specific embedded document or media metadata first, then content-based analysis (including document preprocessing such as `pdftotext` before an LLM request), then filename/path analysis by LLM, and finally the placeholder's generic fallback. A placeholder may omit sources that cannot produce that kind of value, but a lower-priority source must not override a value from a higher-priority source.

## Suggestion Model

A **Suggestion** is a proposed change to the filesystem. There are two user-facing suggestion types:

| Type | Description | Example |
|------|-------------|---------|
| `move` | Relocate and/or rename a file or directory | `/home/alice/Downloads/photo.jpg` -> `/home/alice/Pictures/2024/photo.jpg` |
| `delete` | Remove a redundant file | `/home/alice/Desktop/copy.pdf` (duplicate of `/home/alice/docs/original.pdf`) |

Move destinations are produced by the destination format string for the target's Asset Kind (see "Destination Path Format Strings"). The format string resolves to the complete new path relative to the platform's home directory (or an absolute path). A filename placeholder controls whether the moved object keeps its original name or is renamed.

**Suggestion properties**:
- `target`: Absolute path to a file or directory
- `type`: `move` or `delete`
- `new_path`: Proposed absolute destination (for `move` only)
- `reason`: Human-readable explanation
- `metadata`: Structured data supporting the suggestion (e.g., "extracted date: 2024-03-15"). Move suggestions carry file and EXIF facts when the extractors found them: `extracted_date`, `date_source`, `size`, `bitrate_kbps`, `duration_secs`, `page_count`, `camera`, `title`, `author`, plus GPS coordinates (`latitude`, `longitude`) and the reverse-geocoded `location` name when one resolved. Delete suggestions carry `duplicate_of` (the retained keeper) and `size`.
- `dependent_operations`: Filesystem operations that are required because this suggestion is executed; these operations are not independently selectable

**One suggestion per target**: homelinter produces exactly one top-level suggestion per file or repository unit. (If no reorganization is warranted, no suggestion is generated.) Moves apply only to files and repository roots. Ordinary directories are discovered and analyzed for structure inference, but they are not themselves targets of reorganization or deletion suggestions. Internal analysis may evaluate multiple candidates, but only the single best primary operation is surfaced. Dependent symlink repairs belong to their parent suggestion and do not violate this rule. Suggestions never modify file contents.

**Dependent operations**: a move suggestion may have zero or more dependent `repair_symlink` operations, and a delete suggestion may have repairs attached when deleting the target would otherwise leave a confirmed symlink broken. Each dependent operation identifies an absolute symlink path, the symlink target recorded during analysis, and the replacement target to write after the parent operation. The primary operation is the dependency's cause. The parent and all of its dependent repairs form one execution unit: dependent operations are mandatory members of the execution plan, are executed only if the parent operation succeeds, are not independently selectable, rejectable, or executable, and never run if the parent is rejected, deselected, or otherwise excluded from the execution batch. A symlink repair is never surfaced or executed as an independent suggestion. Dependent operations do not change file contents of either the moved/deleted object or the symlink target.

Interactive clients may select top-level suggestions; dependent operations always follow their parent. CLI execution and export operate on the complete generated batch.

### Deduplication Semantics

- **Content verification**: SHA-256 is the only hash algorithm used. The full-content SHA-256 hash is the only hash that determines membership in a duplicate group. For files larger than the configured threshold, a fast-path SHA-256 hash is computed from the first 1MB concatenated with the last 1MB and the total file size encoded as an 8-byte little-endian unsigned integer (in that order, with no separator). The fast path is a shortlist only: files whose fast-path hash already matches are queued for a full-content hash, and only files whose verified full-content hashes match are grouped and considered for `delete` suggestions. The fast path never establishes a duplicate relationship or a delete suggestion by itself. A full-content hash is computed on demand whenever a group's membership must be confirmed.
- **Duplicate groups**: a duplicate group is the set of files whose verified full-content hashes match. Each group is reported as a finding during analysis. For every redundant file in a group (all except the preferred keeper), homelinter suggests `delete`, and the preferred keeper may be moved to a new suggested path and name when its inferred destination differs from its current path. The keeper is a move candidate under exactly the same resolution, validation, and safety rules as any non-duplicate file: it receives a `move` suggestion if and only if the resolved `new_path` differs from its current absolute path (per "Destination Path Format Strings"), the move passes destination resolution and batch validation, and no metadata-preservation suppression applies to the group. A keeper whose move is suppressed or rejected receives no suggestion and keeps its current location, exactly like any other file whose move fails validation; a blocked sibling delete never blocks or changes the keeper's move. A redundant copy never receives a move or any other suggestion: its one top-level suggestion is the `delete`, so "one suggestion per target" holds even when the same copy would otherwise be a move candidate. Delete suggestions are always file-level: deduplication operates on file content hashes, and a repository root is an atomic unit that is never a deletion candidate.
- **Conflict resolution**: when duplicates are confirmed, the oldest file (by creation time; fallback to modification time) is preferred. Newer copies receive exactly one suggestion each.
- **Metadata preservation**: if the newer duplicate has better metadata (e.g., more complete EXIF), homelinter emits a warning and suppresses the executable suggestion for the conflicting files. The TUI notifies the user; in non-interactive modes, the warning is logged and the files are left untouched until manually reviewed.

## Directory Structure Inference

In the absence of explicit configuration, homelinter infers the user's preferred organization by scanning existing top-level directories in `$HOME`:

1. Detect standard directories (`Documents`, `Downloads`, `Pictures`, `Music`, `Videos`, `Desktop`, `.config`, etc.)
2. Identify user-created organizational patterns (year-based folders, project folders, client folders)
3. Map Asset Kinds to inferred destinations using folder names and existing file distributions
4. Infer destination format strings from recurring structural patterns: a `Pictures/2024/03` layout suggests `Pictures/%Y/%m/%f`, a flat `Music/<artist>` layout suggests `Music/%a/%f`, and a flat directory with no recurring structure infers `Projects/%f` or another `%f`-suffixed directory for the matched kind
5. Confidence weighted by: number of existing files, consistency of pattern, recency of use

With **explicit configuration**, `target_paths` directly maps Asset Kinds to destination format strings and overrides inferred defaults. Platform user-directory APIs contribute only to inferred defaults (precedence level 3) and never override explicit `target_paths`; `xdg-user-dir` is used on Linux where available, with platform-appropriate equivalents on macOS and Windows.

## Destination Path Format Strings

A destination format string is a printf-style template that determines the new path of a discovered object, relative to `$HOME` or absolute. Every Asset Kind has exactly one destination format string at any point, resolved by the standard precedence (see "Configuration"). A format string resolves to the complete new path of the moved object; the final `new_path` of a move suggestion is the resolved path.

### Syntax

- The resolved string is interpreted relative to the platform's home directory unless it uses the host platform's absolute-path syntax, in which case it may point anywhere allowed by the operating system, including outside the home directory and on another filesystem.
- `%%` emits a literal percent sign.
- Every other `%` must be followed by a supported placeholder character. An unknown placeholder is an error at configuration and parse time: it is rejected rather than emitted literally, so typos fail fast instead of producing surprising paths.
- Placeholders may repeat and may be combined in any order, for example `Pictures/%Y/%m/%f` or `Music/%a/%Y/%t.%e`.
- A format string must contain at least one placeholder and must name the target file. A format string with no placeholder names a fixed file and is an error at configuration and parse time. A format string that resolves to an empty path, or whose final segment is empty, is also rejected: it must not end with a path separator or an empty value. The original filename is never appended implicitly, and a trailing `/` is not special.
- The original filename is optional. Omitting the filename placeholders (`%f`, `%F`, `%n`, `%e`) discards the original name and names the target from metadata alone, for example `Pictures/%Y/%m/%s` or `Pictures/%s_%Y%m%d`. Date and metadata placeholders always produce a non-empty value, so such a format still yields a valid filename.

### Placeholders

| Placeholder | Meaning | Primary source | Fallback |
|---|---|---|---|
| `%Y` | four-digit year | extracted date (EXIF date taken, ID3 date, document date) | modification time |
| `%y` | two-digit year | extracted date | modification time |
| `%m` | two-digit month (`01`-`12`) | extracted date | modification time |
| `%d` | two-digit day (`01`-`31`) | extracted date | modification time |
| `%H` | two-digit hour (`00`-`23`) | extracted date | modification time |
| `%M` | two-digit minute (`00`-`59`) | extracted date | modification time |
| `%S` | two-digit second (`00`-`59`) | extracted date | modification time |
| `%t` | title | document title, ID3 title, EXIF title, repository name | content-derived description, then filename/path analysis by LLM, then filename stem |
| `%a` | author / artist | document author, ID3 artist, EXIF artist, or for a document with no embedded author the text description provider's inference | `unknown` |
| `%l` | location | GPS metadata reverse-geocoded to a place name, reduced to the city (or a finer neighbourhood for configured cities) | `unknown` |
| `%s` | subject / description | the selected content-modality provider (per-modality override for the kind, else base); kinds without a content source have no provider | filename stem |
| `%c` | camera model | EXIF camera model | `unknown` |
| `%h` | remote host | repository remote URL host (e.g. `github.com`) | empty (segment omitted) |
| `%o` | remote owner | repository remote URL owner/username, the first path segment after the host (e.g. `github.com/<owner>/<repo>`) | empty (segment omitted) |
| `%k` | asset kind | classification | always available |
| `%f` | original filename, including extension | original filename | always available |
| `%F` | original filename, lowercased with whitespace replaced by hyphens | original filename | always available |
| `%n` | filename stem (original filename without its extension) | original filename | always available |
| `%e` | extension without the leading dot; empty when the file has no extension | original extension | always available |

**Fallbacks**: format-string metadata follows the general order: kind-specific embedded metadata, content-based analysis, filename/path analysis by LLM, and then the placeholder-specific generic fallback. Date placeholders use an extracted date from that hierarchy and fall back to the file's modification time, which is always available. Date extraction may also inspect the filename as an implementation detail when higher-priority metadata does not provide a date. Recognized filename forms are implementation-defined and may include examples such as `IMG_20240315_153045.jpg`, `PXL_20240315_153045123.jpg`, `2024-03-15.jpg`, and `20240315.jpg`; this list is illustrative, not exhaustive. Filename-derived dates must be parsed conservatively and must not override a date from a higher-priority source. `%t` uses document or media title metadata first, then content-derived subject matter or description, then filename/path analysis by LLM, and finally the filename stem. `%s` uses content-derived description before filename/path analysis by LLM and falls back to the filename stem (the original name without its extension) when no description is available, whether no provider was applicable or a provider failed. `%l` uses reverse-geocoded location when available, and falls back to embedded GPS as comma-separated decimal degrees with six fractional digits when coordinates exist but reverse geocoding is unavailable or fails. `%a` uses embedded artist/author metadata (EXIF, ID3, PDF, or office-document/container) first, and for a document with no embedded author falls back to the text description provider's inference before the generic `unknown`. The remaining camera placeholder (`%c`) falls back to `unknown` when metadata is absent or extraction failed.

Filename placeholders derive from the original filename, so a format string can rename a file while preserving or dropping its extension, for example `%t.%e` or `%n_%Y%m%d.%e`: `%f` keeps the original case and spacing, while `%F` normalizes the same value to lowercase with whitespace replaced by hyphens.

### Resolution & Sanitization

- The format string is resolved per target at suggestion-generation time using the metadata collected during analysis; the resolved path is captured in `new_path`, and later metadata changes do not alter an already-generated suggestion. A resolved path equal to the target's current absolute path (same parent and filename) produces no move.
- The parent directory of the resolved path is created before a move; exported scripts emit `mkdir -p` for it. Clobber prevention, cross-filesystem handling, and deduplication apply identically to resolved and static destinations.
- The same format string applies to files and repository units alike; a repository move uses the `repository` kind's format. A rename produced by a filename placeholder is a `move` like any other, subject to the same validation, safety, and audit rules.
- Suggestions are batch-validated after generation, before display, export, or execution (see "Execution & Safety" - Suggestion Batch Validation).
- The format string is user configuration and is validated rather than sanitized: it must resolve to a non-empty path whose final segment is a filename, and interpolated segments cannot change the path structure.
- Interpolated values are never trusted as raw path text. Each metadata value (`%t`, `%a`, `%l`, `%s`, `%c`, `%F`) is lowercased (Unicode-aware) and every run of whitespace (spaces, tabs, newlines) is replaced with a single hyphen; date placeholders contain only digits, `%k` is its canonical lowercase Asset Kind name, and literal text including `%%` is unchanged. A normalized segment therefore can never introduce a path separator or traversal, regardless of source metadata.
  - Path separators (`/`, `\`), NUL, control characters, and platform-invalid characters are replaced with `_`.
  - Leading and trailing whitespace, hyphens, and dots are trimmed after normalization.
  - A value that is empty, or normalizes to `.` or `..`, is replaced with `unknown`.
  - Example: `San Francisco` -> `san-francisco`; `Sunset over the River` -> `sunset-over-the-river`.
- Filename placeholders (`%f`, `%n`, `%e`) are the only exception, preserved verbatim with case and spacing intact; `%F` is normalized like a metadata value above and is not exempt. This is safe because an existing host filename cannot contain a path separator, NUL, control character, or resolve to `.`/`..`, so it is always a valid destination segment on the same filesystem. Host limits still apply: a resolved destination whose final segment is unsafe - empty, `.`/`..`, containing a path separator, NUL, or control character, or (on Windows) colliding with a reserved device name (`CON`, `PRN`, `NUL`, etc.) - is rejected by batch validation with an `invalid_destination_segment` reason before any operation. The platform's own path-length limit is not pre-checked; an overlong destination fails at execution like any other operation failure. When `%e` is empty, an immediately preceding `.` is omitted, so `%n.%e` degrades to the bare stem.
- `%l` location: resolved by the configured geocoder to a city name (or a finer neighbourhood for cities listed in `geocoder.neighbourhood_cities`; see below); `unknown` when the geocoder is disabled, the file has no GPS data, or it fails. Raw coordinates are never interpolated into paths.
- `%s` subject: resolved using the general metadata precedence: embedded metadata where applicable, then the selected content-description provider (per-modality first, then base), then optional filename/path analysis by LLM. The filename stem (the original name without its extension) is used when no applicable source is available or a provider fails. Kinds without a content source (`repository`, `archive`, `data`, `executable`, `other`) never receive a content-description provider - the base `[description]` entry is the default for the content-modality kinds only - so `%s` falls back to the filename stem for them; filename-based description remains future work and is not performed today. For documents, the file is first preprocessed to plain text with the configured extraction tool before a content-description request; if no extraction tool is available, the description service receives no text and `%s` falls back to the filename stem. The request wording comes from the provider's `prompt` template (the built-in wording is the default; see the `[description]` schema), and the full source path plus any extracted text are appended verbatim after the rendered template (path always, text whenever an extractor produced it), so the model sees the file identity and the document content directly. The built-in prompt wording tells the model not to repeat the file type, medium, path, or folder names, and to respect the requested word budget. The completion is used as `%s` exactly as the model returns it - no post-processing or truncation, because the model is trusted to pick the most important words; the word budget is enforced only through the prompt wording and the vendor token limit. Cache lookups and keys follow the persistent cache under `Configuration > cache`, and provider selection follows the `[description]` schema. When a document's format uses `%a` and its embedded metadata (EXIF, PDF, or container) carries no author, the worker also asks the same text description provider who the author is: the request uses a fixed built-in prompt (not the configurable description prompt), carries the file path and extracted text like a description request, and is cached under its own kind so it never collides with the description entry. The answer populates `%a`, and a failure degrades to `unknown`.

### Defaults

When neither configuration nor inference supplies a format string for a kind, the opinionated defaults apply:

| Kind | Default format string |
|---|---|
| `document` | `Documents/%a-%t.%e` |
| `photo` | `Images/%Y-%m-%d-%H-%M-%S-%l-%s.%e` |
| `video` | `Videos/%Y-%m-%d-%H-%M-%S-%l-%s.%e` |
| `audio` | `Audio/%a-%t.%e` |
| `repository` | `Projects/%o/%n` |
| `archive` | `Archives/%f` |
| `data` | `Data/%f` |
| `executable` | `bin/%f` |
| `other` | `Other/%f` |

The archive/data/executable/other defaults preserve the original filename explicitly via `%f`; the media and document defaults name the target from metadata and keep the original extension via `%e`.

### Examples

Config file:

```toml
[target_paths]
photo = "Pictures/%Y/%m/%f"
audio = "Music/%a/%Y/%t.%e"
```

`photo = "Pictures/%Y/%m/%f"` moves `~/Downloads/IMG_0001.jpg` to `~/Pictures/2024/03/IMG_0001.jpg`, keeping the original name via `%f`. `audio = "Music/%a/%Y/%t.%e"` renames `~/Downloads/Song Title.mp3` to `~/Music/artist/2024/song-title.mp3`, deriving the artist and title from ID3 tags and normalizing them per the sanitization rules (here the ID3 artist `%a` = `Artist` and title `%t` = `Song Title` are lowercased and hyphenated to `artist` and `song-title`).

CLI override:

```text
homelinter --target-path photo='Pictures/%Y/%m/%f' --target-path video='Videos/%Y/%m/%f'
```

Locations and descriptions:

```toml
[target_paths]
photo = "Pictures/%Y/%m/%l/%s.%e"

[geocoder]
enabled = true
service = "nominatim"
language = "en"

[description.image]
enabled = true
model = "<vision-model-name>"
```

With this configuration, `~/Downloads/IMG_0001.jpg` moves to `~/Pictures/2024/03/paris/sunset-over-the-river.jpg`: the location is reverse-geocoded by Nominatim to `paris` and the description is generated by the LLM as `sunset-over-the-river`.

## Configuration

Configuration contains persistent preferences only. The scan root is deliberately not part of the configuration file schema: it is selected for each scan or interactive session. A CLI invocation may select it with `--root <path>`, and the TUI/SPA may select it through their per-scan/per-session controls. If no per-scan root is supplied, homelinter uses the host platform's home directory. A scan root supplied in a config file is rejected rather than silently ignored.

### Precedence

The single preference resolution order (highest to lowest) is:

1. Per-scan or per-session settings, including CLI options and interactive-client controls
2. Config file values
3. Inferred values from system tools and home directory structure
4. Opinionated defaults

This same order governs destination format strings, config-file selection details below, and all other resolvable preferences.

### Config File Resolution (highest to lowest precedence)

1. Path specified by the `--config <path>` option
2. The per-user configuration path for the host platform, typically `$HOME/.config/homelinter/config.toml` on Linux and macOS and `%APPDATA%\homelinter\config.toml` on Windows
3. The system-wide configuration path for the host platform, typically `/etc/homelinter/config.toml` on Linux and macOS and the Windows system-wide application-data location on Windows

The configuration files are merged from lowest to highest precedence with a deep merge: nested tables and mappings are merged by key recursively; all other values - scalars and arrays - are replaced by the higher-precedence value; and an empty table contributes nothing, so a higher-precedence file cannot wipe an entire lower-precedence table (there is no "replace this whole table" syntax in TOML, and none is implied). Under these rules, a user `[description.image]` entry composes with a system `[description]` base, a higher-precedence `document_extraction` entry replaces only the argv array for its format key, and `scan.exclude_paths` is replaced wholesale rather than unioned, allowing a higher-precedence file to drop a system pattern. The same deep-merge semantics apply to `PUT /config`, which merges submitted values into the existing file rather than replacing it; in addition, a `null` value in the submitted document deletes the key from the merged file, which is how a form-based client resets an option to its default. The same semantics also apply to the optional per-scan configuration overlay in `POST /session/scan`: a partial configuration document merged over the effective configuration for that scan only, with `null` deleting the key for that scan, never persisted (see "Service API"). A missing explicitly requested config file or a malformed config file is an error; an absent default config file is ignored.

Resolution is implemented over an injectable search chain (`ConfigChain` in the model crate): the system-wide and per-user files below the explicit file. Production always resolves the host chain; the service accepts a chain override in `ServiceOptions` (like the home and app-data overrides) so embedded and test runs can supply an empty chain, keeping config-file behavior hermetic on hosts that have their own per-user configuration.

### Configuration File Schema

The configuration file is TOML. The supported schema is:

```toml
[target_paths]
# Each key is an Asset Kind: document, photo, video, audio, repository,
# archive, data, executable, or other. Values are destination format
# strings (see "Destination Path Format Strings") resolving to a path
# relative to $HOME or an absolute path; a value must contain at least one
# placeholder and name the target file. The original filename is optional
# (omit %f/%F/%n/%e to discard it and name the target from metadata).
document = "Documents/%a-%t.%e"
photo = "Images/%Y-%m-%d-%H-%M-%S-%l-%s.%e"

[geocoder]
enabled = false
service = "nominatim" # "nominatim", "photon", "google", "mapbox", or "custom"
endpoint = "https://nominatim.openstreetmap.org"
api_key = "<optional-api-key>"
language = "en"
neighbourhood_cities = [] # city names whose %l uses the finer neighbourhood level
user_agent_contact = "<optional-contact-string>" # appended to the User-Agent; recommended for public services such as Nominatim

[description]
# Default provider used for content-modality kinds without an explicit
# per-modality entry; kinds without a content source (repository, archive,
# data, executable, other) never use a description provider. A per-modality entry takes precedence, including
# enabled = false. LLM transport settings live directly on the provider
# entry; a per-modality entry inherits any unset key from this base section.
enabled = false
service = "ollama" # "ollama", "openrouter", or "custom"
endpoint = "http://localhost:11434"
model = "<model-name>" # required when the provider is enabled
api_key = "<optional-api-key>" # required by openrouter; sent as a bearer header
max_words = 4 # optional; maximum words in the generated description
# Optional prompt template; placeholder {words}; {{ }} for literal braces.
# Unset = built-in wording for at most max_words words; the full path (with the
# filename) and any extracted document text are always appended to the template.
# prompt = "Describe the file in at most {words} words"
max_image_dimension = 1024 # optional; longest side in px for photos sent to a vision backend (0 = forward original bytes)
rate_limit_ms = 0 # optional; minimum ms between requests (0 = none)

# Optional per-modality overrides can be added as [description.text],
# [description.image], [description.video], or [description.audio].
# An explicitly disabled per-modality entry suppresses the default for that modality;
# an entry that only sets `model` re-specializes just that modality's backend.

[document_extraction]
# Per-format text extraction commands used to preprocess documents
# before description generation. Each value is an argv array; `{path}`
# is replaced by the absolute input path and must be a complete argument
# of its own; plain text is read from stdout. Commands are never interpreted by a shell. Defaults
# are selected at runtime from available tools, like fd; an empty
# array disables extraction for that format.
pdf = ["pdftotext", "-layout", "{path}", "-"]
docx = ["pandoc", "{path}", "-t", "plain"]
odt = ["pandoc", "{path}", "-t", "plain"]
epub = ["pandoc", "{path}", "-t", "plain"]

[duplication]
large_file_threshold = 104857600

[scan]
exclude_paths = [".cache/**", "build/**"]
# Definition of "hidden" on Windows only; ignored on Linux/macOS, where
# hidden always means dotfile names. "explorer", "attribute", or "dotfiles".
hidden_definition = "explorer"

[output]
format = "pretty" # "pretty", "json", or "toml"

[cache]
# The persistent metadata cache is stored in the platform's standard
# per-user application-data directory by default.
enabled = true

[limits]
# Bounds on external calls and tool output. Every value is optional;
# an absent value uses the implementation default, and the defaults are
# never infinite. Overrunning a limit degrades to the normal failure
# behavior (e.g. `%s` falls back to the filename stem) rather than stalling the scan.
provider_timeout_seconds = 120
tool_timeout_seconds = 30
tool_stdout_max_bytes = 16777216
```

**Secrets**: API keys for `geocoder` and `description` providers must never be written to logs or included in suggestions.

**Validation split**: Configuration validation distinguishes structural errors from readiness advisories. Structural errors, like an unknown placeholder, an invalid or unknown key(including per-modality description keys and `scan.root`/`scan.hidden`), an unknown service name, an explicitly empty value, a malformed extraction command, an empty exclusion pattern, or a zero limit, make the config unusable and are rejected everywhere (a 422 on `PUT /config`, a failed load at startup), regardless of how the config was produced. Cross-field readiness conditions, like an enabled provider missing a companion value(a `model` for an enabled description entry, an `api_key` for `google`/`mapbox` geocoders or `openrouter` descriptions, or an `endpoint` for the `custom` provider), or carrying a value its service ignores(an `api_key` for `nominatim`/`photon` geocoders or `ollama` descriptions), are reported as `warnings` rather than errors:the config still loads and scans proceed with degraded metadata(the affected placeholder degrades to its fallback: `%l` to `unknown`, `%s` to the filename stem) until the missing value is supplied. This keeps the file valid while a client edits fields one ata time:enabling a geocoder or description provider never transiently invalidatesethe config mid-edit, so field-at-a-time saves never fail validation. Warnings are surfaced in `GET /config`,the `PUT /config` response,andthe `config.updated` event asa `warnings` object(field path to human reason);the SPA renders them inline onthe scan setup page,and in the configuration view.

Schema rules:

- `target_paths` is an optional mapping from Asset Kind to destination format string. Resolved paths emitted in suggestions are always absolute. An unknown placeholder, a format string with no placeholder, a format string that resolves to an empty path, or a format string that ends in a path separator is a configuration error. A resolved destination whose final segment is empty is rejected at suggestion time and produces no move. The original filename is never appended implicitly. Values must not contain `~` or environment-variable references; such a value is a configuration error, consistent with the API's rejection of `~`, relative, and environment-variable path inputs. The operating system's permissions and execution safety checks remain applicable. See "Destination Path Format Strings".
- `description` is an optional provider configuration with optional per-modality overrides. The base `[description]` provider is the default for every content-modality kind (document, photo, video, audio); a per-modality override (`[description.text]`, `[description.image]`, `[description.video]`, `[description.audio]`) re-specializes one content modality (`text` for documents, `image` for photos, `video` for videos, `audio` for audio), so a vision model can be configured for images only while a cheap text model describes documents. `description.max_image_dimension` caps the longest side in pixels of photos forwarded to a vision backend: a larger photo is decoded, downscaled (aspect preserved), and re-encoded as JPEG before the request, so a 12 MP phone photo reaches the model as a ~1 MP image and the vision model processes far fewer image tokens - the dominant inference cost on CPU; a photo already within bounds is forwarded unchanged (no re-encode loss), an undecodable photo (e.g. HEIC, truncated) is forwarded raw, and `0` disables the cap. `description.prompt` (on the base entry or a per-modality override, falling back to the base then to the built-in default) is an optional prompt template rendered with the `{words}` (the effective `max_words`) placeholder; unknown placeholders are a structural error, and `{{`/`}}` escape literal braces. The full source path (which carries the filename) and any extracted document text are always appended verbatim after the rendered template (as `Path:`/`Content:` blocks), so they need no placeholders, a custom template cannot omit them (referencing `{name}` or `{path}` is itself a structural error), and photos receive the path but never text content. Only those four modality keys may be used as per-modality keys; any other key is rejected. A document whose format uses `%a` and whose embedded metadata carries no author is also asked of the same text description provider (with a fixed built-in prompt) to infer the author; the answer populates `%a` and a failure degrades to `unknown`.
- `geocoder.enabled` defaults to `false`. `service` accepts `nominatim`, `photon`, `google`, `mapbox`, or `custom`; `endpoint` defaults to the service's public endpoint (see "Provider Plugins"), and `language` is an optional ISO 639-1 code passed through to the provider. `user_agent_contact` is an optional string appended to the User-Agent sent with geocoder requests (`homelinter/<version>` plus contact); it does not join the cache key, and is recommended for public services that require identification. When enabled, `api_key` is required by `google` and `mapbox`, ignored by `nominatim` and `photon`,and optional for `custom`. Missing required keys,ignored keys,and a `custom` geocoder enabled without an `endpoint` are warnings,not errors(see "Validation split"),so enabling a geocoder mid-edit never fails the save; `%l` resolves to `unknown` until the provider works. `geocoder.rate_limit_ms` is an optional non-negative number of milliseconds that paces any service as an additional floor on its built-in pacing: the effective interval is `max(built-in, rate_limit_ms)`, so it can only slow requests down. For `custom`, which has no built-in pace, it starts from zero. A `custom` service implements the neutral reverse-geocode contract described under "Provider Plugins". `geocoder.neighbourhood_cities` is an optional list of city names and controls how much of the provider's structured address is rendered into `%l`. By default `%l` renders the settlement (city, town, village, hamlet, or municipality) and falls through to the state/province/county and then the country when the provider does not populate it, and finally to the provider's free-text display name, so a provider that returns only free text still fulfills `%l`. When the provider's city component matches an entry (case-insensitive, trimmed), the finest level the provider supplies (quarter, suburb, district, or neighbourhood) is used instead: the intent is that frequently visited cities read at neighbourhood level while trips away from home read as a recognizable city name. Localities are resolved deterministically from the provider's structured address at cache-write time; no LLM is involved, and `geocoder.neighbourhood_cities` joins the geocode job's cache key so a change re-resolves locations (see "Cache Implementation").
- `document_extraction` is an optional mapping from document format (`pdf`, `docx`, `odt`, `epub`, and any other format key) to an argv array. The first element is the executable and subsequent elements are arguments; the literal `{path}` must occur exactly once and must be a complete argument of its own (an argument that embeds `{path}` with other text is a configuration error), and the standalone argument is replaced by the absolute input path. Commands are executed directly without a shell, so paths and metadata cannot inject shell syntax. An empty array disables extraction for that format. A command with no executable, more than one `{path}`, an argument that embeds `{path}` with other text, or any unsupported placeholder is a configuration error. Defaults are selected at runtime from available tools, following the fd pattern: `pdftotext` for PDF, `pandoc` for office and ebook formats (the runtime default argv mirrors the shipped `defaults.toml` entries). When no external tool is on the PATH, in-process readers run as the final fallback (`pdf-extract` for PDF; native zip/XML readers for docx/odt/epub - see "Tooling Strategy"), so an unconfigured install still extracts text without external tools. External commands run with a bounded timeout and stdout size limit governed by `[limits]` (`tool_timeout_seconds`, `tool_stdout_max_bytes`), receive a minimal sanitized environment, and are terminated if they exceed either limit; the built-in readers apply the same stdout cap and refuse oversized files, so both paths fail identically. Non-zero exit status, timeout, output-limit failure, unavailable tools, or a failed or unsupported built-in read are treated as extraction failure; the description service receives no text and `%s` falls back to the filename stem while analysis continues.
- Deduplication always uses SHA-256 for both fast-path and full-content hashes. The hash algorithm is fixed and is not configurable.
- `duplication.large_file_threshold` is a non-negative integer number of bytes and defaults to `104857600` (100 MiB).
- `scan.exclude_paths` is an optional array of host-platform glob patterns applied to primary discovery. It does not exclude the home-wide symlink repair pass. Each pattern is evaluated against the normalized absolute path of a discovered object, so matching is deterministic and independent of the discovery order. A pattern containing a path separator matches the relative path from the scan root to the object (for example `build/**` excludes `build` and everything below it, and `cache/vendored/**` excludes that subtree); a pattern without a path separator matches the final path component at any depth (so `*.hs` excludes every Haskell source anywhere under the root). Both `*` and `**` are supported: `*` matches within a single path component and never crosses a path separator, while `**` crosses component boundaries, including zero components; a trailing `/**` matches a directory and everything below it. Patterns are applied only to primary discovery and are never applied to the home-wide symlink repair pass or to repository internals, which are atomic units. VCS ignore rules never affect primary discovery. Excluding a directory excludes everything below it; directory symlinks are excluded along with what they would label, because their contents are never traversed anyway.
- `scan.hidden_definition` is a valid, persistent configuration key defining what counts as hidden on Windows only: `explorer` (default) - an object is hidden when it has the Hidden or System file attribute; `attribute` - the Hidden attribute alone; `dotfiles` - names beginning with a dot, matching Unix semantics. On Linux and macOS the key is ignored: hidden always means dotfile names (see "File Classification"). It is a definition, not a session selection: unlike `scan.root` and `scan.hidden`, which remain invalid configuration keys, `scan.hidden_definition` is accepted.
- `output.format` defaults to `pretty` and accepts only `pretty`, `json`, or `toml`.
- The persistent metadata cache is enabled by default and is stored in the platform's standard per-user application-data directory. It stores successful geocoding, description, and author-inference results, cache keys, provider metadata, schema/version information, and timestamps. Failed, timed-out, or unavailable provider requests are not cached as successful results and may be retried on a later scan. A provider response is not refreshed while its complete cache key remains unchanged, ensuring that repeated scans produce the same user-readable location or description rather than renaming a file because an external response changed. Cache writes are atomic and concurrency-safe; a corrupt or incompatible cache is ignored or rebuilt without preventing a scan. Cache contents may contain extracted text, descriptions, locations, provider identifiers, and content-derived identifiers, so the cache is user data and must be protected with normal per-user file permissions. The cache must not store API keys or full source files. Cache invalidation and clearing are exposed through the CLI and service API.
- `description.max_words`, `description.max_image_dimension`, `description.prompt` (effective template for the modality), `geocoder.neighbourhood_cities`, and the effective `[limits]` values join the cache entry key (see "Cache Implementation"), so changing any of them invalidates cached descriptions and locations and triggers fresh provider requests.
- `scan.root` and `scan.hidden` are not valid configuration keys. The scan root and the hidden setting are session-scoped, exactly like the per-scan API inputs; a config file that sets either is rejected rather than silently ignored.

### Cache Implementation

The cache is implemented as a single SQLite database file (`cache.db`) in the platform's standard per-user application-data directory. The database uses WAL (Write-Ahead Logging) mode for atomic writes and concurrent-safe read/write access.

**Schema**:

- `meta` table: key-value pairs storing schema version, creation timestamp, and metadata. Keyed by text string; values are text.
- `entries` table: cached query results. Columns:
  - `cache_key` (BLOB, PRIMARY KEY): SHA-256 hash of the complete request input (see "Cache Key Construction").
  - `kind` (TEXT, NOT NULL): entry type (`geocode`, `description`, `author`, `filename_analysis`).
  - `provider` (TEXT, NOT NULL): provider identifier (e.g., `description:ollama:mistral` or `nominatim:openstreetmap`) plus a hash of provider-specific settings (backend, model name, endpoint URL, language code, per-modality overrides). Settings are hashed to ensure that config changes invalidate cached entries.
  - `input_hash` (BLOB, NOT NULL): hash of the content-derived input (extracted text, file content, GPS coordinates, or path). Used for diagnostics and index optimization; not used for cache lookup (the `cache_key` is the sole lookup key).
  - `value` (TEXT, NOT NULL): the cached result (location name, description, or analysis).
  - `created_at` (TEXT, NOT NULL): ISO-8601 timestamp of insertion.
  - Index on `(kind)` for discovery and stats queries.

**Cache key construction**:

The cache key is computed as SHA-256 of a canonical serialization of the request tuple: `(kind, provider_id, provider_settings_hash, input_hash)`. Each field is length-prefixed as an 8-byte little-endian integer followed by its content, ensuring unambiguous serialization. This composition guarantees that any change to the provider configuration or input invalidates the key and forces a cache miss, while identical inputs always produce the same key and therefore deterministic cache hits.

**Concurrency**:

Multiple processes may access the cache simultaneously. SQLite in WAL mode permits concurrent readers and serialized writers. Inserts into `entries` are write-once (same key, same value): if a concurrent process has already inserted an entry, the second insert either succeeds (WAL serialization order) or fails with `SQLITE_CONSTRAINT`; both outcomes are safe because the value is identical. A lookup always reads the latest committed value.

**Version and corruption handling**:

On cache open, the implementation reads `meta.schema_version`. If the version is absent, newer, or incompatible with the current code, the entire cache file is replaced and rebuilt from scratch. If any operation raises `SQLITE_CORRUPT` or other unrecoverable errors, the cache file is replaced and rebuilding proceeds, ensuring the scan is never blocked. A corrupt or missing cache is treated identically: the scan proceeds with no cache hits, and new results are written to a freshly created file. Replacement is atomic: a fresh database is written to a temporary name in the same directory and renamed over the old file, so a concurrent process never observes a partially written cache.

**Clearing and eviction**:

`DELETE /cache` (and the `--cache-clear` CLI flag) clears all rows from `entries` and the schema metadata in a single transactional write; it never deletes the cache file. Because SQLite WAL serializes writers, a clear is a transaction like any other: it is safe while another instance holds the file open, safe during a scan (a concurrent scan's later inserts simply repopulate entries after the clear), and safe on Windows, where an open file cannot be deleted. The end state is "cleared, then repopulated from anything written after the clear" - exactly the semantics a pre-scan clear wants. File-level deletion is reserved for the corrupt-or-incompatible rebuild path, where the replacement is atomic (a fresh database written to a temporary name, then renamed over the old one), so concurrent rebuilds last-wins safely. Eviction policies (LRU, TTL, size limits) are not implemented; the cache grows unbounded within filesystem constraints. Future enhancements may add configurable size limits and eviction strategies.

### Provider Plugins

Provider plugins are the single interface for external intelligence: LLM analysis, content descriptions, and reverse geocoding. Each plugin type defines a neutral request/response contract, and built-in backends translate that contract into a vendor's API. A provider failure (non-2xx status, timeout, malformed response) is not a scan error and never blocks a scan: the request is not cached, the scan continues with the placeholder's fallback, and the next scan retries. External calls run with a bounded timeout governed by `[limits].provider_timeout_seconds` (see "Configuration File Schema"); overrunning it is a provider failure like any other.

**Offline request loop**: provider requests are executed on a dedicated background worker that shares no state with the scan phases and cannot block them. The scan enqueues jobs into a FIFO `JobQueue` (`ProviderJob`s carrying the kind, provider identity/settings hash, input, and the scan epoch) and marks the suggestions of units with outstanding jobs `metadata_pending`. The worker drains the queue one job at a time, consults the persistent cache before any network I/O (extracting document text for a description or author job only when the cache misses, per `[document_extraction]`; see "Tooling Strategy"), performs the request under the `provider_timeout_seconds` bound, and stores successes in the cache. Both outcomes resolve a unit's pending count: once the last outstanding request of a unit resolves, its suggestion is updated in place by stable id - rebuilt with the collected provider values on success, kept with generated fallbacks on definitive failure, using the original filename stem without its extension for `%s`, and `metadata_pending` cleared (a rebuild that now says the file stays put removes the suggestion). The batch is re-validated at that point with the real metadata applied (pending suggestions stay excluded), so a collision that only appears once a provider value lands is rejected with a reason, and one that disappears no longer blocks the suggestion. Pending suggestions are excluded from execution (see "Session Model > Execution"). Requests to the same provider are paced by that provider's minimum interval (see "Geocoder rate limiting and identification" under "Provider Plugins"; description providers have no built-in pacing but honor their optional `rate_limit_ms`, and both stay serialized one-in-flight per worker). Results carry the scan epoch that enqueued them; a result whose epoch no longer matches the current scan is dropped without touching the session, so a scan that is cancelled or superseded never receives stale provider updates.

**LLM providers** (`description.service`):

| Backend | Default endpoint | API key |
|---|---|---|
| `ollama` | `http://localhost:11434` | never required; a configured key is sent as a bearer header |
| `openrouter` | `https://openrouter.ai/api/v1` | required (config error when LLM enabled without it) |
| `custom` | user-supplied | never required; a configured key is sent as a bearer header |

Each backend adapts a neutral completion request (the rendered prompt template, the configured `model`, with the word budget mapped to a vendor token limit) to its vendor's API and normalizes the returned completion text into the neutral response. The `ollama` and `openrouter` backends translate into the vendor's native chat-completions schema, mappingthe word budget to a token limit; they do not send the neutral contract on the wire. The neutral contract is what a `custom` backend (and any local gateway speaking it) implements: `POST` to `endpoint` with JSON `{"model": "...", "prompt": "...", "max_words": N}` where N is the effective `[description].max_words`, and a successful response is `{"content": "..."}`. An optional configured `api_key` is sent as an `Authorization: Bearer` header. Servers speaking the neutral contract (including local gateways such as LM Studio) work through the `custom` backend. New LLM vendors are added behind the same interface.

**Description providers** (`[description]`):

Description providers are LLM providers; their transport settings - including the optional `prompt` template rendered with `{words}` (built-in wording when unset; the full source path and any extracted document text always appended) - live directly on the `[description]` entry (or the per-modality override), with per-key fallback to the base entry. Hosted APIs (openrouter) throttle by requests per minute on free tiers and local models can stall on concurrent calls, so an entry can set an optional `rate_limit_ms` minimum interval between request starts of that provider on the worker, mirroring `geocoder.rate_limit_ms` (description backends otherwise have no built-in pacing; `0` means no delay and requests remain serialized one-in-flight per worker regardless).

**Geocoder backends** (`[geocoder]` `service`):

| `service` | API | default `endpoint` |
|---|---|---|
| `nominatim` | OpenStreetMap Nominatim reverse geocoding | `https://nominatim.openstreetmap.org` |
| `photon` | Photon reverse geocoding | Photon default host |
| `google` | Google Geocoding API | Google default host |
| `mapbox` | Mapbox Geocoding API | Mapbox default host |
| `custom` | neutral contract (below) | user-supplied |

Each backend maps a neutral reverse-geocode request (latitude, longitude, optional `language`) to the vendor API and parses its structured address into a neutral locality (`fine`/`city`/`region`/`country` plus a free-text fallback). The neutral contract is `POST` to `endpoint` with JSON `{"latitude": ..., "longitude": ..., "language": "..."}`; a successful response is `{"name": "..."}` and may additionally carry optional `neighbourhood`, `city`, `region`, and `country` fields that take precedence over the free-text `name` when present. Raw coordinates are never interpolated into paths; the selected locality fulfills `%l`.

**Geocoder rate limiting and identification**: requests to a geocoder backend are serialized per backend, never sent in parallel, and are handled by the background provider worker (see "Provider Plugins > Offline request loop") so the scan itself never waits on them. The built-in public services enforce their providers' usage policies: `nominatim` sends at most one request per second and `photon` its documented public rate, so a first scan over many GPS-tagged photos trickles rather than floods the public endpoint; the pace is applied between request starts of the same provider on the worker. Requests identify with a User-Agent of `homelinter/<version>` plus the optional configured contact (`geocoder.user_agent_contact`), as public geocoding services require. `custom` and self-hosted endpoints are the user's responsibility and get the same serialization with no built-in delay unless the optional `geocoder.rate_limit_ms` is set (in milliseconds), which paces them exactly like the built-in public services pace themselves. The setting applies to every service as an additional floor on its built-in pace (`max(built-in, configured)`): a value below the built-in pace is absorbed, a value above it only slows requests further, and the documented public pace can never be exceeded. Rate delays and timeouts compose: a request that overruns `[limits].provider_timeout_seconds` is a provider failure like any other.

## User Interfaces

The service is the only backend. It owns the scan session, configuration, suggestion generation, batch validation, execution, export, and reporting. The CLI, TUI, and SPA are clients of the service and do not implement filesystem operations independently.

The scan root and the hidden-file selection are per-scan or per-session settings in every client, selected by CLI (`--root`, `--hidden`) or the interactive clients' per-session controls; they are never persisted as configuration-file values.

The CLI and the TUI embed one service instance in the same process: they bind a loopback listener on an ephemeral port, build the service in-process, and drive it through the HTTP API described under "Service API > Client Lifecycle". Managed sessions always end with `POST /api/v1/shutdown` followed by a bounded wait and, if the service does not exit promptly, forced termination, so an invocation-owned service never outlives its client. The standalone `web` command serves on an ephemeral port, prints the readiness line `homelinter: listening on 127.0.0.1:<port>` to stdout, and never shuts itself down; it is terminated explicitly. The client contract itself is process-agnostic: a future external client can spawn the same binary and parse that readiness line.

The `web` command launches the same service as a standalone localhost server on an ephemeral port. It remains running after browser disconnects and is terminated explicitly by the user. The SPA communicates with it through REST and Server-Sent Events. The server is localhost-only, unauthenticated, and keeps session state in memory; a restart requires a new scan.

All clients display the same suggestions and previews. Interactive clients may select top-level suggestions, while dependent operations are included automatically. Filtering affects navigation and bulk selection only. The full-screen review experience lives in `homelinter tui`; the plain CLI scans, prints a summary, and points the user at the TUI or the `--execute`/`--export` actions (see "Command-Line Interface Schema"). The TUI requires a terminal of at least 80x24 and refuses to start on a smaller one.

### CLI (Command-Line Interface)

The CLI uses the service lifecycle defined above and supports two modes:

- **Plain scan mode (default)**: scans the selected root, generates suggestions, and prints a summary pointing the user at `homelinter tui` for full-screen review or at `--execute`/`--export` to act without review. It never acts on its own; nothing is executed or exported unless one of those flags is given. See "Command-Line Interface Schema" for the exact flow.
- **Non-interactive mode**: driven by instructions passed as command-line arguments, such as `--execute` or `--export`. These modes run to completion without prompting.

### TUI (Terminal User Interface)

- Full-screen interactive terminal interface built with `ratatui` and `crossterm` (target visual style: ASCII only, no em-dashes or special glyphs).
- Uses the shared service and its standard client lifecycle, including `POST /api/v1/shutdown` on quit.
- Displays suggestion previews and supports filtering and selection of top-level suggestions.
- Requires a terminal with minimum 80x24 dimensions.

Implemented as the `tui` subcommand of the CLI binary (see "Command-Line Interface Schema"), embedding the service in-process exactly like the CLI flow. Screens: a scanning progress view, a two-pane suggestion browser (filtered list plus detail panel; row labels render `name -> dest` with paths relative to the scan root, verbatim up to three segments and ellipsis-truncated to the last three beyond that, matching the SPA's rule), a live execution log, and a report view. It keeps one `GET /events` Server-Sent Events connection as the fast path for live events, and polls `GET /session` every frame as a backstop so the session state stays honest even when the stream is dropped. Command key map (see `tui.rs`): arrows and `j` to move; space to select; `a`/`A` to select all/none; `t`/`k`/`u`/`c` to cycle type, asset-kind, and status filters and clear them; `x` to execute with a confirmation dialog; `e` to export to a script path; `n` to scan another root (Tab toggles hidden); `?` for help; `q`/Esc/Ctrl-C to quit (cancelling an in-flight scan or run first).

### GUI via SPA (Single-Page Application)

- The SPA is served by the standalone `web` service described above.
- It can view and modify live configuration; configuration changes are persisted by the service.
- It communicates through REST and Server-Sent Events.
- It is localhost-only, unauthenticated, and survives browser disconnects; closing the browser does not stop the service.

Implemented under `webui/` as React + TypeScript + Vite, with no client-side router (one page, internal views like the TUI). The typed API client (`webui/src/api.ts`) mirrors the Service API JSON shapes from `crates/model`; pure logic lives in `webui/src/lib/` (`filter.ts` filters, `format.ts` path/shorten/report helpers, `sse.ts` SSE payload parsing) and is unit-tested with Vitest in a node environment (no DOM required). Components render with plain CSS classes only, no inline styles, so the served app stays compatible with a strict Content Security Policy (`script-src 'self'`, `style-src 'self'`, with `frame-src` opened to `https://www.openstreetmap.org` solely for the suggestion detail pane's embedded map preview); the Vite build disables the modulepreload polyfill so `index.html` carries zero inline scripts. The SPA keeps one `EventSource('/api/v1/events')` stream with the browser's built-in reconnect as the fast path for live events, plus an unconditional `GET /session` poll every 2 seconds that refreshes the session object (state, config hashes, counts) and keeps the UI honest when the stream is interrupted or another client changes the configuration. State transitions mirror the TUI's phases: scan form, progress, suggestion browser with filters and selection, live run log, report, and a configuration view. The SPA's suggestion browser is a single selectable list, unlike the TUI's two-pane browser: rows open with a checkbox on the left, and the rest of the row renders as one-line action sentences - `move <path> to <path>` for moves and `delete <path>, duplicate of <path>` for duplicate deletes (the retained keeper read from the suggestion's `duplicate_of` metadata) - with status/metadata-pending/rejected suffixes. Row paths are displayed relative to the scan root: a relative path of at most three segments is shown verbatim, and a longer one is truncated to its last three segments with an ellipsis prefix (`.../2024/03/IMG_1.jpg`); a path outside the scan root (possible for destinations) falls back to the absolute path under the same truncation rule, and the detail pane always shows the full absolute paths. Ticking the checkbox is the only way to change selection; clicking the row body opens a detail pane on the right (it does not toggle selection) with three tabs: preview, location, and metadata. The preview tab renders a browser-native file preview when possible: images use an image element, video and audio use media controls, PDFs and text-like files use an embedded frame. The service exposes these previews only through a same-origin, suggestion-id-based endpoint with an extension allowlist. Unsupported file types still show the proposed action - the source and destination paths for a move, or the deleted target with its keeper shown and labeled `original` for a duplicate delete. The location tab appears only when the suggestion carries GPS coordinates: it shows the coordinates, the reverse-geocoded place name when reverse geocoding succeeded, and an embedded OpenStreetMap map preview. The metadata tab appears only when the suggestion carries file or provider metadata beyond the keeper pointer (EXIF fields like camera, date taken, author; file info like size, bitrate, duration, page count) and renders it as a table. A close button dismisses the pane. The scan dialog is the single place to configure a scan, since it determines the next scan's settings, and it edits the service's persistent configuration directly rather than layering a per-scan overlay: it offers individual controls for the scan root, the hidden-file toggle, and one target-path override input per asset kind (empty uses the configured default) - the three session-scoped parameters, which travel only in the scan request and are never persisted - plus collapsible sections for scan exclusion globs, geocoder and description provider transport (service, endpoint, model, API key, language, contact, neighbourhood cities, rate limit), the duplication threshold, limits, the cache, and per-format document-extraction commands. Every configuration-backed control starts at the effective configuration's value, and each change is persisted to the service as it is made: edits are debounced, computed as a minimal partial document (cleared optional values are sent as `null`, which deletes the key and restores the built-in default), and written through `PUT /config`, so the service's merge, validation, atomic file write, and `config.updated` broadcast remain the single path for durable changes and the SPA and service stay in lock step. The scan request itself therefore carries no `config` overlay; a scan always runs under the configuration that was just saved, snapshotted when the scan is accepted. Failed saves are surfaced inline and retried on the next edit; starting a scan first flushes any pending edit and does not start while a save is failing. When the configuration changes in another client, the form re-seeds every field the user has not touched from the fresh document; touched fields keep their values until their pending save lands, so concurrent edits are never clobbered. API key inputs are write-only (the service never returns them): typing a key persists it to the configuration like any other field, the input clears once stored, an empty field leaves the stored key untouched, and removing a key is done in the config file. The configuration view is a pure live mirror Readiness warnings from the service(the `warnings` object in the config document;see "Schema rules > Validation split")are listed inline under the form when an enabled provider is missing a required value or carries a key its service ignores,so a mid-edit incomplete provider setup guides the user rather than failing the save.  of the service's effective configuration, with no reload or edit actions: it renders the `GET /config` document (plus the config file path and per-kind built-in destination defaults) and refetches it whenever the session's `config_hash` changes, whether announced by the `config.updated` event or noticed by the 2-second poll, so what it shows always matches the service no matter which client changed the configuration. The SPA compares `batch_config_hash` against `config_hash` to warn when the displayed batch is stale; saved changes apply to the next scan (the running scan keeps its snapshot). The view's only actions are clearing the metadata cache and closing; because clearing is destructive (all cached provider results are lost and the next scan re-extracts from scratch), it is not wired directly to `DELETE /cache` but opens an in-page confirm modal (the same `modal-backdrop`/`modal` dialog pattern used for the stale-configuration warning, not a browser-native dialog) whose destructive action is styled distinctly and where cancel, not confirm, receives the initial focus. The built app (`webui/dist`) is served by the `web` command from the same process that serves the API (Option A distribution: no separate frontend server). During development, `vite dev` proxies `/api/v1` to a running `homelinter web` instance (see "Developer Environment").

## Command-Line Interface Schema

The CLI has plain scan and non-interactive modes (see "User Interfaces"). The supported command-line syntax is:

```text
homelinter [OPTIONS]
homelinter web [OPTIONS]
homelinter tui [OPTIONS]
```

Supported options:

| Option | Value | Meaning |
|---|---|---|
| `--config` | `<path>` | Select the configuration file. |
| `--root` | `<path>` | Select the scan root for this invocation only. Relative paths are resolved against the current working directory; the resolved path is converted to an absolute path before scanning. Defaults to the platform's home directory. |
| `--execute` | none | Execute all generated top-level suggestions and their dependent operations immediately, without review. |
| `--export` | `<path>` | Scan and generate suggestions, write the complete executable shell plan, including dependent symlink repairs, to `<path>`, and exit without executing it. |
| `--hidden` | none | Include hidden files and directories in primary discovery. |
| `--format` | `<pretty\|json\|toml>` | Override `output.format` for this invocation. |
| `--target-path` | `<kind>=<path>` | Override the destination format string for one Asset Kind for this invocation. Repeatable; each Asset Kind may be supplied at most once. Takes precedence over `target_paths`. |
| `--cache-clear` | none | Clear the persistent metadata cache before this invocation performs any scan or execution. May be combined with any other mode; alone it runs the default plain scan flow. |
| `-v`, `--verbose` | none, repeatable | Increase structured logging verbosity. |
| `-q`, `--quiet` | none, repeatable | Decrease structured logging verbosity. |

`--hidden` applies only to primary discovery and does not affect the separate home-wide symlink repair pass (see "Symlink Handling"). Like `--root`, it is a session-scoped selection and is never persisted to the configuration file. What counts as hidden is platform-native and on Windows configurable via `scan.hidden_definition` (see "File Classification").

`--execute`, `--export` are mutually exclusive. Each is also mutually exclusive with the default plain scan flow. `--config`, `--root`, and `--export` may each be supplied at most once. `--cache-clear` is orthogonal: it may accompany any mode, including the plain scan flow, and never implies scanning beyond the selected mode. If both `--format` and `output.format` are supplied, `--format` takes precedence. `--target-path` takes precedence over `target_paths` for the kinds it names; supplying the same kind twice is an error, as is an unknown Asset Kind or an invalid format string. Conflicting verbosity options are an error. `--export` takes an output path and fails if the path cannot be created or safely written.

The `web` subcommand launches the standalone service described in "User Interfaces". It accepts common configuration and scan options, including `--config`, `--root`, `--hidden`, `--target-path`, `--cache-clear`, and logging options, but not `--execute` or `--export`; execution is initiated through the SPA. `--port <n>` pins the loopback listener to a fixed port instead of an ephemeral one, so a development SPA proxy (default `HOMELINTER_WEB_PORT` 8787) can target a stable address; an ephemeral port remains the default. A root supplied on the `web` command line pre-seeds the session's default scan root; a per-scan `root` in `POST /session/scan` overrides it for that scan, and with neither supplied the platform home directory applies. The scan root is held in session memory and is not persisted to configuration. `--hidden` and `--target-path` pre-fill the SPA scan dialog: the service exposes them as `scan_defaults` in `GET /session`, and the SPA initializes its scan form from them and then sends `hidden` and `target_paths` per scan request exactly as before.

The `tui` subcommand starts the full-screen terminal interface described in "User Interfaces > TUI". Like `web` it accepts `--config`, `--root`, `--hidden`, `--target-path`, `--cache-clear`,and the logging options, but none of `--execute` or `--export`; those actions are available inside the TUI. `--root` pre-seeds the scan root shown in the scan prompt, `--hidden` pre-seeds the hidden-file toggle, and `--target-path` pre-seeds per-scan target-path overrides; all three are attached to every scan request, matching how the web SPA pre-fills its scan dialog from `scan_defaults`. The TUI embeds the service in-process, refuses to start when stdin is not a terminal or the screen is smaller than 80x24, and sends `POST /api/v1/shutdown` when the user quits.

**Plain scan (no `--execute`)**, default:
1. Scan the selected root and generate suggestions.
2. If no suggestions were generated, print a "no suggestions" notice and exit.
3. Print how many suggestions were generated and point the user at the review and action entry points: `homelinter tui` for full-screen review, `--execute` to apply everything, or `--export <path>` to write an executable plan script. Nothing is executed or exported in this mode.
4. Exit without acting.

Plain scan requires a terminal: when stdin is not a terminal it refuses to run and directs the caller to `--execute` or `--export`, so scripts never get a silent no-op scan.

**Non-interactive mode (`--execute`)**
- Acts without review: no summary pointer is shown.
- Scans, generates suggestions, and executes them immediately.
- Every executed operation is recorded in the audit log; live per-operation events are available over SSE (`execution.op`). The CLI prints the final report: in pretty mode on stdout, and in json/toml modes stdout carries only the final document while progress and notices go to stderr (see "Output & Reporting > Machine-Readable Output").
- `Ctrl+C` terminates the process; already-completed operations are not rolled back.

## Service API

The service exposes a single HTTP API on a localhost-only listener. The CLI and TUI spawn an ephemeral instance and drive it through this API for the duration of the client session. The `web` command serves the SPA from the same process and exposes the identical API to the browser. The CLI, TUI, and SPA are therefore interchangeable clients of one backend contract.

### Transport & Conventions

- **Base URL**: `http://127.0.0.1:<port>/api/v1`. The port is ephemeral. Every path in the endpoint tables below is relative to this base: `GET /health` is served at `GET /api/v1/health`, `POST /execute` at `POST /api/v1/execute`, and so on. The managed CLI embeds its service in-process and polls `/health` until it returns 200; the standalone `web` command prints the readiness line `homelinter: listening on 127.0.0.1:<port>` to stdout for external clients, which parse it and then poll `/health`. The SPA uses the same origin it was served from.
- **Protocol**: HTTP/1.1 REST with JSON request and response bodies (`Content-Type: application/json`), plus one Server-Sent Events stream for push updates. The server is unauthenticated, binds loopback addresses only, and rejects non-loopback Host headers.
- **Paths**: every path in a request or response is an absolute, normalized, host-native path per "Scan Root and Path Representation". The service rejects relative paths, `~`, and environment-variable references in request inputs with a 422 error. The CLI resolves `--root` and `--export` paths to absolute form before calling the API.
- **Errors**: errors are returned as `{"error":{"code":"<machine-code>","message":"<human text>","details":{...}}}` with a status of 400 (bad request), 404 (not found), 409 (conflict with the current session state), or 422 (validation failure).
- **Idempotency**: suggestion IDs are stable within a session and derived from the normalized absolute target path, so re-selecting or re-executing a suggestion is a no-op rather than an error. The service retains the report most recently produced by an execution; a re-run of the same batch returns that stored report when it still corresponds to the executed batch, and is rejected with 409 when the stored report covers a different batch, so idempotency never serves a stale result under a different batch's identity.
- **Concurrency**: one scan and one execution at a time per session. A request that conflicts with the current state, for example starting a scan while an execution is running, returns 409.

### Session Model

A service instance holds exactly one in-memory session. The session state machine is:

| State | Meaning |
|---|---|
| `idle` | No scan has been started. |
| `scanning` | File discovery is in progress. |
| `analyzing` | Metadata extraction and suggestion generation are in progress. |
| `ready` | Suggestions are generated and the batch has been validated. |
| `executing` | An execution is in progress. |
| `done` | Execution finished; the report is available until the next scan. |
| `failed` | A scan or execution failed, or a scan was cancelled; no suggestions or a partial report is available, resolved by a new scan. |

A scan that produces no suggestions reaches `ready` with an empty batch: suggestion counts are zero, and clients present a "no suggestions" notice. The empty batch is a valid validated batch; execution on it is rejected with 400 by the existing empty-selection rule.

State transitions: a new scan replaces the session from `idle`, `ready`, `done`, and `failed`; starting one from `scanning`, `analyzing`, or `executing` conflicts with the in-progress operation and returns 409 (cancel first). A new execution is accepted from `ready` and `done`. And only `ready` is never re-entered from a later state except by a new scan. A scan or execution that fails at any point transitions to `failed`, and the failure's reason and any partial report are retrievable until the next scan replaces them.

**Cancellation**: a `POST /cancel` cooperatively cancels the in-progress scan or execution. A scan stops promptly and transitions to `failed` with a cancellation reason: the discovery walk (primary and home-wide symlink pass) and the analysis loop poll the cancellation flag continuously (per entry and per file), so a cancel lands mid-phase instead of waiting for the whole tree or the full extraction batch to finish; the partial result is discarded and the session reports `failed` with a cancellation reason. An execution finishes the top-level suggestion it is working on, marks the remaining ones `skipped` with a cancellation reason, and transitions to `done` with the partial report. Already-completed operations are never rolled back. Cancelling an idle session is a no-op returning the current state. In `ready`, `done`, or `failed` state nothing is in progress, and cancellation returns 409. A later execution naming only the skipped-from-cancellation suggestions (a different batch identity) executes them fresh, while re-running the full batch returns the stored partial report.

A restart requires a new scan: session state is never persisted. The scan root is captured when the scan starts and is retained with the session (see "Scan Root and Path Representation").

**Guarded actions**: the service rejects requests that contradict the state machine with 409 (a new scan during `scanning`/`analyzing`/`executing`, execute/export outside `ready`/`done`, cancel when nothing is in progress). Clients mirror these guards so the controls are disabled before the user attempts them: the SPA derives disabled states from `GET /session` (scan/start disabled while a scan or execution is in progress, execute/export disabled unless the session is `ready` or `done`, cancel disabled otherwise) and the TUI only dispatches the matching keys in the phase where the action is legal (see "User Interfaces").

**Configuration snapshot per scan**: the service snapshots the effective configuration and the per-scan `target_paths` overrides at the moment a scan is accepted. Every phase of that scan (discovery, analysis, suggestion generation, provider planning) runs against that one snapshot, so a config save during a scan never changes the running work and the published batch reflects a single committed configuration. Edits saved mid-scan affect only the next scan. The session exposes both the live effective config hash (`config_hash`) and `batch_config_hash`, the hash of the snapshot the current batch was scanned under; clients compare the two to flag a stale batch and offer a re-scan before execute or export (see "Service API > Session & Scan").

### Data Types

**Suggestion**:

```json
{
  "id": "s-<sha256-of-normalized-target>",
  "type": "move",
  "asset_kind": "photo",
  "target": "/home/alice/Downloads/IMG_0001.jpg",
  "new_path": "/home/alice/Pictures/2024/03/IMG_0001.jpg",
  "reason": "Moved to the photo archive for 2024-03.",
  "metadata": {"extracted_date": "2024-03-15", "source": "exif"},
  "dependent_operations": [
    {"type": "repair_symlink", "link_path": "...", "recorded_target": "...", "replacement_target": "..."}
  ],
  "selected": false,
  "status": "pending",
  "rejection_reason": null
}
```

- `type`: `move` or `delete`. `new_path` is present only for `move`.
- `dependent_operations`: mandatory members of the parent execution unit. They are never independently selectable, rejectable, or executable; the API rejects any request that names one.
- `selected`: client selection state, maintained by the service.
- `status`: `pending`, `rejected`, `executed`, `failed`, or `skipped`. `rejected` carries `rejection_reason` from batch validation and is never executable or exportable.

**ExecutionReport**: one entry per top-level suggestion with `suggestion_id`, `result` (`executed`, `failed`, `skipped`), per-operation outcomes including dependent `repair_symlink` steps, and `partial` (true when a parent succeeded but a dependent repair failed). The report also records the scan root and platform.

**ScanSummary**: total files scanned in the primary scan, total symlinks indexed for repair, suggestions by type and Asset Kind, and estimated space reclaimed by deduplication.

### Endpoints

#### Lifecycle

| Method & Path | Description |
|---|---|
| `GET /health` | Readiness probe. Returns 200 with the current session state when the service is accepting requests; clients poll until 200. In `failed` state the response includes the failure reason. |
| `POST /shutdown` | Graceful shutdown: stop accepting new work, finish in-flight operations, then exit. Returns 204. The CLI and TUI always call this before exiting and force-terminate the process if it does not exit promptly. |
| `POST /cancel` | Cooperatively cancel the in-progress scan or execution (see "Session Model > Cancellation"). A scan stops promptly - the discovery walks and the analysis loop poll the cancellation flag continuously - and transitions to `failed` with a cancellation reason; a cancelled execution finishes its current top-level suggestion, marks the rest `skipped`, and completes with a partial report. Returns 200 with the resulting state (a no-op when idle); 409 in `ready`, `done`, or `failed` state. |

#### Session & Scan

| Method & Path        | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
|----------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `POST /session/scan` | Start a scan. Body: `{"root": "<abs path, optional>", "hidden": false, "target_paths": {"photo": "Pictures/%Y/%m/%f"}, "config": {"scan": {"exclude_paths": ["Documents"]}}}`. `root` defaults to the platform home directory; `target_paths` overrides the config file's `target_paths` for this scan only. The optional `config` member is a per-scan configuration overlay: a partial configuration document with the same shape and merge semantics as `PUT /config` (tables merge by key, scalars and arrays replace, `null` deletes the key so the built-in default applies) that is merged over the effective configuration for this scan only and never persisted. It is validated exactly like a `PUT /config` document - including the rejection of session-scoped `scan.root`/`scan.hidden` - and an invalid overlay returns 422 before any state change. The effective configuration (with the overlay applied) and `target_paths` are snapshotted when the scan is accepted; a config save during the scan does not affect it (see "Session Model > Configuration snapshot per scan"), and `batch_config_hash` reflects the overlay. Returns 202 with `{"state": "scanning"}`; progress and completion arrive over SSE. A scan started from `idle`, `ready`, `done`, or `failed` replaces the session: previous suggestions, selection, and the execution report are discarded. Starting one from `scanning`, `analyzing`, or `executing` conflicts with the in-progress operation and returns 409. A scan started from `failed` replaces the session and its failure record before scanning begins. A failure during scanning or analysis leaves the session in `failed` state (see "Session Model"). |
| `GET /session`       | Current session: state, scan root, platform, effective config hash (`config_hash`), the snapshot config hash of the current batch (`batch_config_hash`, null until a scan publishes its batch), suggestion counts, selection count, and `scan_defaults` (the SPA scan-dialog pre-fills supplied at service start: `hidden` and `target_paths`; see "Command-Line Interface Schema"). A non-null `batch_config_hash` that differs from `config_hash` marks the batch stale: its suggestions were generated under an older configuration.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |

#### Suggestions & Selection

| Method & Path | Description |
|---|---|
| `GET /suggestions` | List top-level suggestions in deterministic order (normalized absolute source path). Query params: `type`, `asset_kind`, `status`, `selected`, `offset`, `limit`. Returns `{"items": [...], "total": N}`. Filtering affects navigation and bulk selection only; it never changes what is selectable or executable. |
| `GET /suggestions/{id}` | One suggestion including its dependent operations. |
| `GET /selection` | The current selection: `{"ids": [...]}`. |
| `PUT /selection` | Replace the selection exactly. Body: `{"ids": [...]}`. Dependent-operation IDs are rejected. |
| `POST /selection/add` | Add to the selection. Body: `{"ids": [...]}` or `{"filter": {...}}` to add everything matching a filter (bulk selection applies to the filtered set only). |
| `POST /selection/remove` | Remove from the selection, with the same body forms as add. |

Every selection change triggers re-validation of the selected subset (see "Suggestion Batch Validation"); rejected suggestions are marked `rejected` with a machine-readable reason and are excluded from execution and export.

#### Validation, Execution & Export

| Method & Path | Description |
|---|---|
| `POST /validate` | Re-validate a batch. Body: `{"ids": [...]}` (optional; defaults to the selection). Returns `{"valid": true}` or `{"valid": false, "rejected": [{"id": "...", "reason": "..."}]}`. The service also validates automatically on selection change and before every execution or export; this endpoint exists for clients to preview rejection reasons. |
| `POST /execute` | Execute a batch. Body: `{"ids": [...]}` (optional; defaults to the selection). Returns 202 with `{"state": "executing"}`; per-operation events and the final report arrive over SSE. A request with no `ids` and an empty selection is rejected with 400; the CLI `--execute` passes all suggestion IDs explicitly. Already-`executed` IDs are covered by the stored report (idempotency), and `rejected` IDs are excluded from the batch without being an error. A request whose IDs resolve entirely to already-executed or rejected entries executes nothing, returns the normal 202 response, and yields an empty report. |
| `GET /execution` | The execution report for the most recent completed execution; 404 when none exists. Only the most recent report is retained per batch identity: a new execution for the same batch replaces the stored report with the same identity, an execution for a different batch replaces the identity and report, and the next scan discards it. |
| `POST /export` | Generate the platform script for a batch. Body: `{"ids": [...] (optional; defaults to the selection), "path": "<abs path, optional>"}`. With `path`, the service writes the script there, failing with 422 if the path cannot be created or safely written, and returns `{"path": "...", "platform": "linux|macos|windows", "count": N}`. Without `path`, the response body is the script text and the platform and count are returned in the `X-Homelinter-Platform` and `X-Homelinter-Count` headers. |

#### Configuration

| Method & Path | Description |
|---|---|
| `GET /config` | The effective configuration merged per "Configuration > Precedence", plus `config_path` (the resolved config file path, or null when none exists) and `target_path_defaults` (the built-in default destination format for every asset kind). Secrets (API keys) are never included. |
| `PUT /config` | Validate and persist a partial configuration to the config file. The service merges the submitted values into the existing file (deep merge: tables by key, scalars and arrays replaced, empty tables contributing nothing,as under "Config File Resolution");a `null` submitted value deletesethe key from the merged file so the default applies again.The result is validated structurally(unknown placeholders, invalid keys, `scan.root` rejection, invalid per-modality description keys,and so on);readiness conditions(an enabled provider missing a required value, ora key its service ignores)are reported as `warnings` in the response instead,so field-at-a-time edits never fail validation mid-edit,andit is written atomically. Returns 200 with the new effective configuration(including the `warnings` object; see "Schema rules > Validation split")and emits a `config.updated` event.Structural validation failures return 422 with per-field errorsand nothing is written. Changes apply to the next scan:the current suggestion batch is not regeneratedand keeps the settings it was scanned under,and a save during a scan or execution does not alter the running work,which proceeds against the snapshot captured when the scan started.The SPA compares `batch_config_hash` against `config_hash` to warn when the batch is stale (see "Session Model > Configuration snapshot per scan"). |
| `POST /config/validate` | Validate a candidate configuration without persisting it. Returns 200 `{"valid": true, "config": ...}`(the merged effective config,including any readiness `warnings`)or 422 on structural errors.Readiness conditions never fail validation(see "Schema rules > Validation split"). |

#### Cache, Audit & Reporting

| Method & Path | Description |
|---|---|
| `DELETE /cache` | Clear all cached geocoding and description results in a single transactional write (see "Cache Implementation > Clearing and eviction"). Returns 204 and emits a `cache.cleared` event. Never deletes the cache file, and is safe during a scan or with other instances using the cache. |
| `GET /report/summary` | The `ScanSummary` for the current session. |
| `GET /audit` | Audit log entries as a JSON array, newest first, read from the persisted NDJSON audit file (see "Output & Reporting > Logging"). Query params: `since` (ISO-8601 timestamp; entries with a timestamp strictly before it are omitted) and `limit` (default 100, maximum 1000; entries beyond the limit are omitted). Each entry: timestamp, operation type, absolute source path, absolute destination path (if any), reason, and the scan root. |
| `GET /events` | The Server-Sent Events push channel described under "Events (SSE)"; on connect it sends the current `session.state` and, when a batch exists, the current `suggestion.batch` snapshot, then streams live events. |

### Events (SSE)

`GET /events` returns a `text/event-stream` connection. On connect, the service sends the current `session.state` and, when a batch exists, the current `suggestion.batch` snapshot; afterwards it streams live events.

| Event | Data |
|---|---|
| `session.state` | `{"state": "idle" \| "scanning" \| "analyzing" \| "ready" \| "executing" \| "done" \| "failed"}` |
| `scan.progress` | `{"phase": "discovery" \| "symlinks" \| "metadata" \| "hashing" \| "repos" \| "suggestions", "scanned": N, "elapsed_ms": N}` emitted at phase boundaries and, during analysis, at most once per second as completed work items (extracted files under `metadata`, hashed files under `hashing`, repositories under `repos`) so a long extraction or content-hashing pass shows live movement in clients instead of a frozen phase line |
| `scan.complete` | `{"summary": {...}}` |
| `suggestion.batch` | `{"items": [...], "total": N}` emitted after generation and batch validation; re-emitted whenever a `metadata_pending` suggestion is updated in place as its provider requests resolve, with the updated items |
| `batch.validated` | `{"valid": bool, "rejected": [{"id": "...", "reason": "..."}]}` emitted after every selection change and after each provider resolution that newly rejects a suggestion |
| `execution.op` | `{"suggestion_id": "...", "operation": "move" \| "delete" \| "repair_symlink", "status": "started" \| "completed" \| "failed" \| "skipped", "detail": {...}}` |
| `execution.complete` | `{"report": {...}}` |
| `config.updated` | `{"config": {...}}` |
| `cache.cleared` | `{}` |

### Client Lifecycle

The CLI and TUI spawn the service, parse the readiness line, poll `/health` until 200, drive the session through the API, then call `POST /shutdown` and wait for process exit, force-terminating if the process does not exit promptly. An invocation-owned service must not outlive its client. The `web` command serves the SPA at `/` and the API under `/api/v1` from the same process; it never shuts itself down and is terminated explicitly by the user.

The non-interactive CLI modes map directly onto this API: `--execute` scans and executes with all suggestion IDs, and `--export <path>` scans and calls `POST /export` with the resolved path. The plain CLI scan reports through the same endpoints and prints its summary from them; the TUI uses the same endpoints and adds live progress over SSE.

## Tooling Strategy

homelinter shells out to external tools for discovery and metadata extraction:

| Task | Primary Tool | Fallback |
|------|-------------|----------|
| File discovery | `fd` | `find` |
| EXIF extraction | `exiftool` | Built-in `kamadak-exif` (Rust) |
| Audio tag/stream extraction | `exiftool` | Built-in `lofty` (Rust) |
| Document metadata extraction | `pdfinfo` (PDF) | Built-in zip/XML reader (Rust, docx/odt/epub) |
| Document text extraction | `pdftotext` (PDF), `pandoc` (docx/odt/epub) | Built-in `pdf-extract` (PDF) and zip/XML readers (docx/odt/epub) |
| Git info | `git` CLI | - |
| Hashing | Rust `sha2` crate | - |

**Embedded metadata extraction** is implemented as a plug-in chain consulted in fixed priority order per analyzed file, with the first `Some` winning each field; every extractor failure degrades to missing metadata and analysis never fails because of one. `exiftool` is preferred when it is on the PATH and covers photos, videos, and audio with a fixed, small tag list (`-n -json`) for date taken (`DateTimeOriginal`/`CreateDate`/`TrackCreateDate`/`MediaCreateDate`), title (`XPTitle`/`Title`/`ImageDescription`), artist (`Artist`), camera (`Model`/`Make`), signed GPS coordinates, and stream `Bitrate`/`Duration`. Built-in in-process readers follow: `kamadak-exif` for photo containers (JPEG, TIFF, PNG, WebP, HEIF) reads a bounded prefix of each image, and `lofty` for audio/video container tags (ID3v1/v2, FLAC, Vorbis Comments, MP4/M4A, WAV, AIFF) reads tags structurally by header offsets rather than scanning whole files and also reports stream bitrate and duration. Document metadata is extracted by `pdfinfo` for PDFs (title, author, page count, and the raw creation date via `-rawdates`, falling back to plain output on poppler builds without the flag) and by a built-in reader for OOXML/ODF/EPUB containers (`docx` `docProps/core.xml`, `odt` `meta.xml`, `epub` `container.xml` to the OPF file), which surface `dc:title`, `dc:creator`, and `dc:date` from the container XML; only PDFs supply a page count. The extractor trait returns one `EmbeddedMeta` value per file (date taken, title, author, camera, signed latitude/longitude, plus informational bitrate, duration, and page count that no placeholder currently consumes) instead of one method per field. External tools run under the bounded tool timeout and stdout cap from `[limits]` with a minimal sanitized environment.

**Document text extraction** follows the same runtime precedence as the metadata chain: the configured `[document_extraction]` command for the format, then the default tool when it is on the PATH, then in-process readers as the final fallback when no tool is available. The in-process readers are `pdf-extract` (a `lopdf`-based crate) for PDF and native zip/XML readers for the office/e-book containers, using the same `zip` + `quick-xml` stack as the metadata reader: docx `word/document.xml` `w:p`/`w:t` runs (with `w:tab`/`w:br` mapped), odt `content.xml` body paragraphs and headings inside `office:text` (including `text:s` space runs), and epub in spine reading order (`META-INF/container.xml` to the OPF package, then each spine item's XHTML with head/nav/script markup stripped and common HTML entities decoded). No third-party office crate is used: `docx-rs` is write-oriented and no maintained equivalent exists for odt/epub, so one uniform container reader covers all three formats. The built-ins mirror the external discipline: extraction whose text would exceed the `[limits]` stdout cap is a failure (not a truncated success), files over 128 MiB are refused rather than read into memory, the PDF reader runs under a panic guard so a malformed document cannot abort the worker, and results are trimmed with blank-line runs collapsed. Because the built-ins are only a fallback, their output quality (pdf-extract has no layout preservation, the office readers cover body text only) never displaces a configured tool.

**Repository adapters**: repository metadata (remote URLs, default branch, commit count, last activity) is collected by a pluggable per-VCS adapter following the same pattern as the provider plugins in "Provider Plugins". Adapters for `git` (`.git` directory or file), `hg` (`.hg`), and `svn` (`.svn`) map to their platform CLIs (`git`, `hg`, `svn`). The git adapter uses `remote get-url origin`, `symbolic-ref --short HEAD`, `rev-list --count HEAD`, and `log -1 --format=%ct`; the hg adapter uses `paths default`, `branch`, and the tip changeset's rev and `{date|hgdate}` (Mercurial revision numbers are dense local indexes, so tip rev plus one is the changeset count); the svn adapter parses `info --xml` for the working-copy URL and last-changed date, derives the branch name from the URL path when the layout is standard (`trunk`, `branches/<name>`), and leaves commit count empty because svn revisions are repository-global and counting log entries would require a full repository walk. All adapter commands are local-only (no network round trips) and svn runs non-interactive so credential prompts never hang an unattended scan. An adapter whose CLI is unavailable leaves that repository's metadata empty and analysis continues, exactly like other optional tools; new version-control systems are added behind the same interface without changing analysis code.

External tools are optional and platform-specific. The implementation must use platform-neutral Rust APIs for core discovery, hashing, path handling, and execution safety, with external tools used only when available and compatible with the host platform. Windows equivalents may be used for metadata and repository operations, but missing Unix utilities are never errors when a built-in implementation is available. Tools are detected at runtime. If a primary tool is missing, homelinter falls back gracefully. If no suitable tool is available for a metadata type, that metadata is omitted (analysis continues). Hashing is always in-process via the Rust `sha2` crate: it is a core safety operation used by deduplication and move verification, is required on every platform, and never depends on an external binary.

## Execution & Safety

### Guaranteed invariants

These invariants are non-negotiable and testable:

- homelinter never overwrites an existing object.
- homelinter never modifies file contents.
- homelinter never follows or modifies an unsupported link or reparse point.
- A dependent operation runs only after its parent operation succeeds.
- A stale suggestion whose source identity no longer matches is never executed.
- An invalid or colliding batch is never executed.
- Partial completion and failures are always reported accurately.
- Completed operations are never reported as rolled back.

homelinter does not guarantee transactional behavior across multiple filesystem operations. A later failure may leave earlier operations completed. Cross-filesystem moves and dependent symlink repairs can therefore produce partial completion.

### Execution policies

Destructive and lower-safety behaviors are gated behind explicit configuration. The proposed defaults favor safety over completeness. Symlink repair and cross-filesystem moves are not configurable:

- Delete suggestions are executable and exportable whenever they pass the same validation and execution checks as other suggestions.
- Suggestions with `metadata_pending = true` (provider metadata still in flight) are never executed, exported, or counted as executable; they become eligible only after their provider requests resolve (see "Provider Plugins > Offline request loop").
- Cross-filesystem moves always use the verified copy-and-delete protocol described under "Transactional Safety"; there is no configuration to weaken or disable it.
- Symlink repair is always performed under the safe-only semantics described in "Symlink Handling".
- Execution always continues with unrelated suggestions after a failure. A failed suggestion and all of its dependent operations are skipped or stopped, while unrelated suggestions remain eligible.

### Transactional Safety

- Suggestions and their dependent operations are validated immediately before execution (re-check destination, re-stat source, and validate dependent symlink state).
- Moves use atomic rename within the same filesystem, performed with no-replace semantics so the rename fails rather than overwrites if an object exists at the destination: `renameat2` with `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, and `MoveFileEx` without `MOVEFILE_REPLACE_EXISTING` on Windows. Where the platform cannot express a no-replace rename, the destination is re-validated immediately before the publish and files are published through a hard link, which fails on an existing target; if no safe no-replace publish is available, the move fails rather than risks an overwrite.
- A cross-filesystem move of a file follows a verified copy-and-delete protocol whose shape mirrors the kernel-based copy strategy of `mv`: the copy phase uses `copy_file_range(2)` where available (so the kernel performs the copy, and network filesystems such as NFS/SMB may execute it server-side), falling back to `sendfile(2)` and then to a plain read/write loop when unsupported; Windows uses its file-copy API. Unlike `mv`, which copies into place without verification, the protocol is: copy to a temporary file in the destination filesystem, `fsync` it, verify it against the source with the full-content SHA-256 hash, publish the copy by renaming the temporary file to its final name within the destination filesystem using the no-replace rename above, and delete the source only after the copy is published. At every interruption point, either the intact source or a complete verified destination exists at its final path; the source is never removed until the verified destination is in place.
- Move verification always uses the full-content SHA-256 hash at every file size. The fast-path hash is a deduplication shortlist mechanism only and never verifies a move, so corruption in any part of a large file is detected before the source is removed.
- A cross-filesystem move of a directory (a repository root) applies the same verified protocol to the whole tree, and the source tree is untouched until the copy is complete and verified:
  1. Create a uniquely named temporary directory in the destination filesystem.
  2. Copy the tree into it file by file with the verified file protocol above, preserving file metadata under host-platform semantics, preserving empty directories and the directory structure, and recreating contained symlinks and supported link types from their raw targets without following them; a contained link type that cannot be recreated safely fails the move rather than being dropped or copied as a regular file.
  3. `fsync` the copied files and directory entries.
  4. Verify structural completeness: the copy's relative path set must exactly match the source's, and every copied file's full-content hash must match its source.
  5. Publish the copy by renaming the temporary directory to the final destination name with the no-replace rename above; the rename stays within the destination filesystem.
  6. Delete the source tree only after the publish succeeds.
  A failed or interrupted tree move leaves either the intact source or the complete verified copy at the temporary path, and the temporary directory's path is reported rather than silently removed, because the executor cannot always prove the source is intact afterward. A source-tree deletion that fails after a successful publish is reported as partial completion: the destination remains complete and the leftover source paths are reported.
- A parent move or delete is executed before its dependent symlink repairs. Dependent repairs are mandatory members of the parent execution unit and are executed only if the parent operation completes successfully.
- If a parent operation fails validation or execution, its dependent operations are skipped. Already-completed operations are not rolled back.
- If a parent operation succeeds but one of its dependent symlink repairs fails, the result is reported as partially completed. The executor attempts the remaining safe dependent repairs, while unrelated suggestions remain eligible for execution.

### Suggestion Batch Validation

After suggestion generation, homelinter validates the complete top-level suggestion batch before presenting it in the TUI or SPA. The same validation runs again for every selected subset before export or execution, because interactive selection can change operation dependencies and path conflicts.

Validation uses a deterministic ordering based on normalized absolute source paths and compares paths using the host filesystem's collision rules, including case folding where applicable. It rejects every suggestion in a collision set when multiple moves have the same destination. It also rejects any batch containing a destination that conflicts with another planned source, a delete target, or a dependent operation, unless the plan explicitly defines a safe, order-independent sequence, and rejects any move whose resolved destination has an unsafe final segment (see "Resolution & Sanitization") with an `invalid_destination_segment` reason. Rejected suggestions are reported with a collision reason. Existing destination checks, source identity checks, and symlink precondition checks remain mandatory immediately before execution. A batch with rejected suggestions is never presented as executable, exported, or executed; the rejected suggestions and machine-readable reasons are included in reporting.

Suggestions whose provider metadata is still in flight (`metadata_pending`) are excluded from batch validation: their destinations are built on fallback metadata, so a collision may disappear once the provider result arrives. They are re-validated against real metadata when their last provider request resolves (see "Provider Plugins > Offline request loop"), and a newly triggered rejection is broadcast as a `batch.validated` event. Validation is monotonic: a suggestion marked rejected is never un-rejected, and re-validation never re-admits a full-batch rejection. Every rejection carries its machine-readable `rejection_reason` on the suggestion itself, so the API, TUI, and SPA can show why each suggestion was rejected.

### Platform Script Export

Export uses the same validated execution plan and dependency order as the native executor. Linux and macOS exports are POSIX shell scripts; Windows exports are PowerShell scripts. The exported language is determined by the host platform at export time and is not configurable, so a Linux or macOS export is always POSIX and a Windows export is always PowerShell. They contain absolute paths, create move destinations, perform moves and deletes, and include dependent symlink repairs only after their parent operation succeeds. Scripts use platform-safe quoting and are not portable between operating systems.

Exported scripts cannot guarantee that the filesystem still satisfies its preconditions when the script is later run. They repeat the same safe defaults as the executor: they do not embed destructive operations beyond the plan, they abort on unexpected destination or source state rather than blindly overwriting, and cross-filesystem moves follow the same verified copy-and-delete protocol as the native executor. The native executor remains authoritative for validation and safety.

### Race Condition Handling

- Files are re-stat immediately before execution. File identity is the platform object identity: Unix device number plus inode (`st_dev`, `st_ino`); Windows volume serial number plus file index. A re-stat that finds a different size or a different object identity than analysis invalidates the suggestion and re-queues it for re-analysis. If only mtime changed, the source is re-checked and the suggestion may still execute when content identity is unchanged. When the platform cannot provide an object identity (for example FAT-style filesystems reporting a zero inode), identity degrades to the normalized absolute path plus a metadata signature (size and mtime), and the degraded identity is treated as unverifiable: the suggestion is conservatively re-analysed rather than executed.
- A file lock is not held during interactive approval (TUI/SPA); if the file changes during approval, the user is notified and the suggestion is refreshed.

## Output & Reporting

### Scan Summary

Pretty-printed table or JSON/TOML containing:
- Total files scanned in the primary scan
- Total symlinks indexed for repair across the platform's home directory
- Suggestions by type and Asset Kind
- Estimated space reclaimed (for deduplication)

### Machine-Readable Output (`--format json|toml`)

`output.format = "json"` or `"toml"` (or `--format json|toml`) applies to the `--execute` and `--export` outputs only: the plain scan summary is always pretty. In json/toml mode the CLI emits a single structured document to stdout containing the union of the service data types that apply to the mode: the scan root, platform, `ScanSummary`, the full `Suggestion` batch when suggestions are shown, and the `ExecutionReport` when executing with the pieces absent from the mode omitted. Per-operation progress during `--execute` is structured logging and goes to stderr in these modes, so stdout carries exactly one parseable document. The shape is exactly the corresponding "Service API" data types, not a new schema; `summary` matches `ScanSummary` as returned by `GET /report/summary`. TOML output omits optional and null-valued fields (TOML has no null); JSON output follows the API representation. Diagnostics and structured logging continue to go to stderr in every format. "Pretty" is the human-facing default and has no schema of its own.

### Logging

- Structured logging to stderr (level controlled by `-v` / `-q`)
- Optional log file in the platform's standard per-user application-data directory
- Audit log of all executed suggestions appended to the platform's standard per-user application-data directory as `audit.ndjson`, alongside the cache database
- Audit log format: newline-delimited JSON (NDJSON), one entry per executed suggestion, containing timestamp, operation type, absolute source path, absolute destination path (if any), and reason. The record also includes the absolute scan root used for discovery.
- The audit log is append-only, persists across sessions and restarts, and is never session-scoped; it is not rotated or rewritten. Its file name is fixed (`audit.ndjson`), and `GET /audit` reads it directly (see "Service API > Cache, Audit & Reporting").
- Exported scripts do not append to the audit log: they run outside the service and their later execution is not audited.

## Implementation Notes

- Written in Rust
- Async I/O for scanning and metadata extraction (tokio)
- Modular workspace: `model`, `scan`, `analyze`, `suggest`, `execute`, `service`, `clients` crates plus the `homelinter` CLI binary crate
- The `service` crate is the shared backend implemented by the locally running web server; the CLI, the TUI, and the SPA (`webui/`) are clients of it

### Implementation status

The core pipeline and the HTTP service API are implemented and exercised by an end-to-end suite (unit and integration tests across all crates, including service API tests that drive real scans, executions, exports, config PUT, cache clear, and an SSE connection over TCP):

- `model` owns the error envelope, host-native path rules, Asset Kind classification, printf-style format strings, TOML/JSON config deep-merge and validation, suggestion/session types, and SHA-256 hashing.
- `scan` implements primary discovery, the home-wide symlink pass, ToolSet detection for optional external tools, and repository atomicity.
- `analyze` fills provenance and content hashes, runs the embedded metadata extractor chain (photos/videos via exiftool then the built-in EXIF reader, audio via exiftool then the built-in `lofty` reader, documents via `pdfinfo` and the built-in container reader), collects repository metadata through the per-VCS adapters (`git`, `hg`, `svn`), extracts filename dates, and deduplicates (keeper = oldest copy).
- `suggest` generates move and delete suggestions with dependent symlink repairs; delete suggestions always carry the recorded full-content hash required at execution and export time.
- `execute` implements no-replace rename (kernel-atomic `renameat2(RENAME_NOREPLACE)` on Linux, checked fallback elsewhere), hash-verified delete, dependent symlink repair, verified cross-filesystem copy-and-delete with publish-then-delete ordering, and platform script export (POSIX `sh` / PowerShell).
- `service` implements the full REST + SSE API under `/api/v1`, the single-session state machine, batch validation, config merge-and-write, the SQLite cache, and the append-only audit log. Async-accept endpoints (`POST /session/scan`, `POST /execute`) return 202 and deliver their outcomes over SSE.
- `clients` is a small hyper-based HTTP/1.1 + SSE client used by the CLI and the TUI.
- The `homelinter` binary embeds one service instance in-process for the managed CLI flows, the `tui` subcommand, and `web`, per the lifecycle described under "User Interfaces".
- `homelinter tui` is the full-screen terminal client (ratatui + crossterm): scanning screen, two-pane suggestion browser with type/kind/status filters and selection, live execution log, report view, read-only configuration view, cache clearing, confirmation and text-input dialogs, and an 80x24 minimum terminal check. Keys: arrows/`j` move, space select, `a`/`A` select all/none, `t`/`k`/`u`/`c` filters, `x` execute, `e` export, `n` scan another root, `g` open configuration, `C` clear the cache from the configuration view, `?` help, `q`/Esc/Ctrl-C quit (cancelling in-flight work first). The TUI mirrors session configuration hashes and surfaces service failure reasons, and it asks for confirmation before executing or exporting a batch whose configuration hash differs from the scan-time hash.

### Documented seams

- **Windows platform primitives**: symlink replacement is refused on non-Unix platforms rather than guessed; the exported PowerShell marks reparse-point repair as a seam and defers to a native run; `same_filesystem` conservatively assumes a shared volume so an unsafe copy is never attempted. No-replace rename falls back to a checked rename where the kernel API is unavailable.
- **Provider plugins**: Built-in transports implement the neutral `custom` completion and reverse-geocode contracts, Ollama chat completions, OpenRouter chat completions, and the Nominatim, Photon, Google, and Mapbox reverse-geocoding APIs. Public geocoders use their native response formats, Nominatim requests include the configured identifying User-Agent contact, and API-key-backed services validate and send credentials only for their configured request. For photos with the ollama transport, the raw image bytes are base64-encoded into the chat message's `images` field (bounded by a 20 MiB cap) so vision-capable models analyze the pixels: the filename stays in the prompt as a cheap prior, the cache key for a vision job joins the photo's content hash so an edited image is re-described, and a cached text-only guess is never served for a vision job (and vice versa). Photos larger than the effective `max_image_dimension` (default 1024 px on the longest side) are decoded, downscaled with aspect preserved, and re-encoded as JPEG at quality 85 before the request, so the model sees far fewer image tokens - on CPU this turns multi-minute inferences into seconds; undecodable or already-small photos are forwarded unchanged. Every ollama request carries a 30-minute `keep_alive` so the model stays resident between scans, and a scan that plans a vision job spawns a fire-and-forget text-only warmup chat (no timeout) so the model load happens out-of-band instead of eating the worker's provider timeout on the first image request; the per-machine `[limits] provider_timeout_seconds` should cover cold load plus inference (CPU-only vision models can take several minutes). Image and video recognition providers remain future work (beyond still-image photos via ollama). The plugin seams (extractor/config schemas, ToolSet detection, cache) are in place. Embedded metadata extraction ships for photos and videos (`exiftool`, then the built-in `kamadak-exif` reader), for audio (`exiftool`, then the built-in `lofty` reader), and for documents (`pdfinfo` for PDFs and a built-in docx/odt/epub reader); the offline provider request loop (job queue, rate-limited worker, cache-first fetch, in-place suggestion refresh with epoch gating and the `metadata_pending` flag) and its format-gated job planner are implemented in the service. Document text for description prompts is extracted by the worker per `[document_extraction]` (configured commands, default tools when on the PATH, then the in-process readers - `pdf-extract` for PDF and zip/XML readers for docx/odt/epub), only on a provider cache miss so a scan and a cached description are never blocked on text extraction. A document whose format uses `%a` and has no embedded author is asked of the text description provider under its own cache kind, reusing the same extracted text. When a provider or extractor is unavailable the affected metadata is omitted and analysis continues.
- **TUI and SPA**: both are built. The TUI runs in-process with the service; the SPA is served from `webui/dist` by the `web` command (see "GUI via SPA").

## Developer Environment

- Nix flake (`flake.nix`) with Fenix provides the pinned Rust toolchain via `rust-toolchain.toml` (single source of truth, also usable with `rustup`), rust-analyzer, `pkg-config`, and developer tools (`cargo-nextest`, `cargo-audit`, `cargo-deny`, `taplo`).
- `direnv` entry point is `use flake`; pairing with `nix-direnv` caches the shell for fast reloads.
- The toolchain version is pinned in `rust-toolchain.toml`; the Fenix `sha256` in `flake.nix` is updated when that file changes.
- C dependencies go in `devShells.default.buildInputs`: pkg-config -s dev outputs land in `PKG_CONFIG_PATH` and stdenv rpaths linked libs automatically. `LD_LIBRARY_PATH` is set only on demand (crates that dlopen `.so` files by name, e.g. game engines); see the commented example in `flake.nix`.
- - The SPA (`webui/`) is developed node-free for the Rust workspace: `cargo build` and the test suites never run the JS toolchain, and `webui/node_modules` and `webui/dist` are git-ignored. `webui` pins its own toolchain (`package.json` + `pnpm-lock.yaml`, with the `packageManager` field pinning pnpm the way `rust-toolchain.toml` pins Rust); run `pnpm install`, `pnpm test` (Vitest), `pnpm build` (Vite), and `pnpm typecheck` there. The dev loop: start `homelinter web --port 8787`, then `pnpm dev` with the Vite proxy (default `HOMELINTER_WEB_PORT` 8787) pointing at the running service.
