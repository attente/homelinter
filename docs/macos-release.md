# Building a macOS release without a Mac

`homelinter` can be released for macOS entirely from GitHub Actions.
The workflow runs on a GitHub-hosted macOS runner, so nobody needs a Mac, Xcode, or a local Apple toolchain.
There are two routes:

| Route | Requirements | Result |
|-------|--------------|--------|
| Unsigned (default) | A GitHub repository | A universal DMG that macOS asks the recipient to approve once. |
| Signed and notarized | An Apple Developer Program membership and six repository secrets | A universal DMG that opens without a Gatekeeper warning. |

The workflow picks the signed route only when all six Apple secrets are present.
With none of the secrets it builds the unsigned route.
With some but not all of the secrets it fails early, so a half-configured signed release never ships.

A free Apple ID can create an Apple Development certificate through an Xcode Personal Team.
That certificate is not a Developer ID Application certificate and does not enable notarization, so it does not remove the Gatekeeper warning for other users.
The unsigned route below does not need any Apple account.

## 1. Bump the version

Update the version in `crates/desktop/src-tauri/tauri.conf.json`.
Keep `crates/desktop/src-tauri/Cargo.toml` in sync, and refresh the desktop lockfile without compiling:

```sh
cargo metadata --format-version 1 \
  --manifest-path crates/desktop/src-tauri/Cargo.toml > /dev/null
```

The Tauri bundle name and the app's About panel use the config version, not the Git tag.
Commit the version, then tag and push:

```sh
git add \
  crates/desktop/src-tauri/tauri.conf.json \
  crates/desktop/src-tauri/Cargo.toml \
  crates/desktop/src-tauri/Cargo.lock
git commit -m "release: 0.2.0"
git tag v0.2.0
git push origin main
git push origin v0.2.0
```

The `v*` tag starts `.github/workflows/macos-release.yml`.

You can also run the workflow manually from the Actions tab with Run workflow.
A manual run uploads the DMG as a workflow artifact but does not create a GitHub release, because the ref is not a tag.
If the Run workflow button is missing, push the workflow file to the default branch first.

## 2. What the workflow produces

For either route the workflow:

- Checks out the repository on `macos-14`.
- Installs pnpm 10 and Node 22.
- Installs Rust 1.85.0 with both `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- Detects whether all six Apple secrets are configured, none are, or only some are.
- Runs `pnpm dlx @tauri-apps/cli@2.11.1 build --target universal-apple-darwin`.
- The Tauri `beforeBuildCommand` installs the SPA dependencies and rebuilds `webui/dist`.
- Writes `INSTALL-macOS.txt` with the steps for the selected route.
- Uploads the DMG and install notes as an artifact, and attaches them to the GitHub release when the ref is a `v*` tag.

The DMG is written on the runner at `crates/desktop/src-tauri/target/universal-apple-darwin/release/bundle/dmg/`.
The artifact is named `homelinter-macos-universal-unsigned` or `homelinter-macos-universal-signed`.

## 3. Unsigned route: no Apple account

This is the default route.
Do not add any `APPLE_*` secrets.
If signing secrets from an earlier attempt still exist, remove them, for example with `gh secret remove APPLE_CERTIFICATE --repo OWNER/REPOSITORY`, and repeat for the other five names.

Tag the release as described above.
The workflow builds the universal app without a signing identity.
Tauri therefore skips code signing and notarization, and the DMG contains an unsigned app.
The app still runs normally after the recipient approves it once; the approval is per app, not a system-wide Gatekeeper change.

### What the recipient sees

Downloading the DMG applies macOS's quarantine attribute.
On the first launch, macOS blocks the app with a message such as "Apple could not verify homelinter is free of malware" or "homelinter is damaged and can't be opened".
This is expected for a build that is neither signed with a Developer ID nor notarized.
It does not mean the download failed.

### Approval on macOS 15 and later

1. Open the DMG and drag `homelinter` to Applications.
2. Double-click `homelinter` once.
3. Dismiss the blocked-app dialog.
4. Open System Settings, then Privacy & Security, then scroll to Security.
5. Find the message about `homelinter` and click Open Anyway.
6. Authenticate with Touch ID or the account password, then confirm Open.

Control-clicking the app no longer provides the old bypass on macOS 15 and later; use the Open Anyway button.
If the dialog says the app is damaged rather than unverified, use the Terminal alternative below.
After the first approval, later launches open normally.

### Approval on macOS 14 and earlier

1. Open the DMG and drag `homelinter` to Applications.
2. Control-click or right-click `homelinter` and choose Open.
3. Confirm Open in the dialog.

The app then opens normally on later launches.

### Terminal alternative

If the Open Anyway button is missing, the recipient can remove the quarantine attribute for this one app and open it:

```sh
xattr -dr com.apple.quarantine /Applications/homelinter.app
open /Applications/homelinter.app
```

Add `sudo` if the app is owned by another account.
This changes only the quarantine attribute of `homelinter.app`.
Do not run `sudo spctl --master-disable`; disabling Gatekeeper system-wide is broader than this app needs.

### Notes for the recipient

- Ad-hoc signing (`codesign --sign -`) does not change this outcome: it identifies no developer, so a downloaded build is still blocked until approved.
- The approval applies to the installed copy; replacing the app with a new unsigned build repeats it.
- macOS still shows the normal folder-access prompts for Desktop, Documents, Downloads, removable volumes, and network volumes, because the app is not sandboxed.
- The DMG is universal, so it runs on both Apple Silicon and Intel Macs.
- If macOS says the app is damaged even after removing quarantine, download the DMG again and compare the app's architecture with `file /Applications/homelinter.app/Contents/MacOS/homelinter`.

## 4. Optional signed route

Use this route only if you have an Apple Developer Program membership and want the app to open without a Gatekeeper prompt.
It adds the `Developer ID Application` certificate and Apple's notarization service.
Keep all six secrets together; the workflow rejects a partial configuration.

### 4.1 Join the Apple Developer Program

Enroll at <https://developer.apple.com/programs/>.
An individual membership costs 99 USD per year; organizations need a D-U-N-S number and may take longer to approve.
After enrollment, open <https://developer.apple.com/account> and copy the 10-character Team ID from Membership Details.

You need a Linux shell with OpenSSL to create the certificate signing request and the `.p12` bundle.
You do not need a Mac for this step.

### 4.2 Create the certificate signing request

Generate a private key and a CSR on Linux:

```sh
mkdir -p "$HOME/homelinter-signing"
cd "$HOME/homelinter-signing"

openssl genrsa -out developer-id.key 2048
openssl req -new \
  -key developer-id.key \
  -out developer-id.csr \
  -subj "/CN=Developer ID Application/O=Your Name/C=US"
```

Use your own name or organization in `O`.
The private key is not sent to Apple during this step.

### 4.3 Download the certificate and Apple's intermediate

In the Apple Developer portal, open Certificates, Identifiers & Profiles, then Certificates.
Create a new `Developer ID Application` certificate by uploading `developer-id.csr`.
Download the resulting `.cer` file as `developerID_application.cer`.

Convert it to PEM:

```sh
openssl x509 -inform DER -in developerID_application.cer -out developer-id.crt
```

Download Apple's Developer ID intermediate certificate so the `.p12` carries the full chain:

```sh
curl -fsSLO https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
openssl x509 -inform DER -in DeveloperIDG2CA.cer -out DeveloperIDG2CA.pem
```

Most current Developer ID certificates are issued under the G2 intermediate.
For an older certificate issued under G1, use `DeveloperIDCA.cer` from <https://www.apple.com/certificateauthority/> instead.

### 4.4 Build the `.p12`

Bundle the private key, the leaf certificate, and the intermediate into one password-protected `.p12`:

```sh
export P12_PASSWORD='choose-a-strong-password'

openssl pkcs12 -export -legacy \
  -inkey developer-id.key \
  -in developer-id.crt \
  -certfile DeveloperIDG2CA.pem \
  -name "Developer ID Application" \
  -out developer-id.p12 \
  -passout pass:"$P12_PASSWORD"
```

If `-legacy` is unavailable, or macOS later fails with a MAC verification error, use the explicit compatibility options instead:

```sh
openssl pkcs12 -export \
  -inkey developer-id.key \
  -in developer-id.crt \
  -certfile DeveloperIDG2CA.pem \
  -name "Developer ID Application" \
  -certpbe PBE-SHA1-3DES \
  -keypbe PBE-SHA1-3DES \
  -macalg sha1 \
  -out developer-id.p12 \
  -passout pass:"$P12_PASSWORD"
```

Read the certificate common name; it becomes `APPLE_SIGNING_IDENTITY`:

```sh
openssl x509 -in developer-id.crt -noout -subject -nameopt sep_multiline
```

Use the full common name, for example `Developer ID Application: Your Name (TEAMID)`.
Base64-encode the `.p12` without line breaks:

```sh
base64 -w0 developer-id.p12 > apple-certificate.b64
```

On a Mac, use `base64 -i developer-id.p12 -o apple-certificate.b64` and confirm the file has no line breaks.

### 4.5 Create the notarization app-specific password

Sign in to <https://appleid.apple.com> with the Apple ID that owns the team.
Enable two-factor authentication if it is not already enabled.
Open Sign-In and Security, then App-Specific Passwords, and generate a password labelled `homelinter notarization`.
Copy it immediately, because Apple shows it only once.
This is `APPLE_PASSWORD`, not the Apple ID account password.

### 4.6 Add the repository secrets

In GitHub, open the repository, then Settings, Secrets and variables, Actions, and add all six repository secrets.
Alternatively, use `gh secret set` after `gh auth login`.

| Secret | Value |
|--------|-------|
| `APPLE_CERTIFICATE` | The contents of `apple-certificate.b64`, with no line breaks. |
| `APPLE_CERTIFICATE_PASSWORD` | The password used for `developer-id.p12`. |
| `APPLE_SIGNING_IDENTITY` | The certificate common name, for example `Developer ID Application: Your Name (TEAMID)`. |
| `APPLE_ID` | The Apple ID email address that owns the team. |
| `APPLE_PASSWORD` | The app-specific password from step 4.5. |
| `APPLE_TEAM_ID` | The 10-character Team ID from Membership Details. |

With `gh`:

```sh
gh secret set APPLE_CERTIFICATE --repo OWNER/REPOSITORY < apple-certificate.b64
gh secret set APPLE_CERTIFICATE_PASSWORD --repo OWNER/REPOSITORY
gh secret set APPLE_SIGNING_IDENTITY --repo OWNER/REPOSITORY
gh secret set APPLE_ID --repo OWNER/REPOSITORY
gh secret set APPLE_PASSWORD --repo OWNER/REPOSITORY
gh secret set APPLE_TEAM_ID --repo OWNER/REPOSITORY
```

The commands without a redirected file prompt for the value, so the secret is not left in shell history.
Do not commit `developer-id.key`, `developer-id.p12`, or `apple-certificate.b64`.
Keep `developer-id.key` in encrypted storage so you can reuse it when the certificate is renewed, or create a new key pair with the new certificate.

### 4.7 Verify the signed build

Download the DMG from the release page or the workflow run.
On the Mac that will use the app, open the DMG, drag `homelinter` to Applications, and launch it.
Gatekeeper should open it without a right-click bypass or an Open Anyway step.

For an explicit check:

```sh
spctl -a -vv /Applications/homelinter.app
codesign --verify --deep --strict --verbose=2 /Applications/homelinter.app
xcrun stapler validate /Applications/homelinter.app
```

`spctl` should report `source=Notarized Developer ID`.
The build itself does not need a Mac; only these verification commands do.

## Troubleshooting

### Unsigned build

- `Apple could not verify ...`: the app is unsigned or unnotarized and needs the one-time Open Anyway approval described in step 3.
- `homelinter is damaged and can't be opened`: remove the quarantine attribute with `xattr -dr com.apple.quarantine /Applications/homelinter.app`, or approve it in Privacy & Security; a genuinely truncated download needs a fresh copy.
- The Open Anyway button is absent: launch the app once first, then reopen System Settings, Privacy & Security; the button appears under Security for the blocked app.
- `bad CPU type in executable`: confirm the artifact name contains `universal` and that the workflow used `--target universal-apple-darwin`.
- Folder access prompts appear on the first scan: this is normal macOS TCC behavior for a non-sandboxed app; the purpose strings are in `Info.plist`.

### Workflow

- The workflow fails with `Set all six APPLE_* secrets`: the repository has a partial signing configuration; add the missing secrets or remove all six to build unsigned.
- The workflow succeeds but no GitHub release appears: only a `v*` tag publishes a release; a manual run uploads the artifact instead.
- The DMG has the wrong version: update `tauri.conf.json` and retag, because the config version wins over the tag.

### Signed route

- `MAC verification failed during PKCS12 import`: regenerate the `.p12` with `-legacy`, or with the explicit SHA1 and 3DES options.
- `unable to build chain to self-signed root`: include `DeveloperIDG2CA.pem` with `-certfile`.
- `The specified item could not be found in the keychain`: `APPLE_SIGNING_IDENTITY` does not match the certificate common name exactly.
- Invalid notarization credentials: use the app-specific password for `APPLE_PASSWORD`, and confirm `APPLE_TEAM_ID` is the team that owns the certificate.
- A free Apple Development or Personal Team certificate does not work for notarization; use a `Developer ID Application` certificate.
- A missing or expired certificate: create a new Developer ID Application certificate, rebuild the `.p12`, and update all three certificate secrets.

## Security notes

- The unsigned route stores no signing credentials in GitHub.
- The signed route stores the certificate and password as Actions secrets; anyone with both can sign as your team.
- Revoke the app-specific password at <https://appleid.apple.com> if it leaks.
- Revoke the certificate in the Apple Developer portal if the private key leaks.
- The app is distributed outside the Mac App Store and is not App Sandboxed, because it must scan the home directory and move files to user-chosen destinations.
- The unsigned route does not ask users to disable Gatekeeper; it asks them to approve this one app.
