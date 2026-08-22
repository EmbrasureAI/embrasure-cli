# Windows release

Windows ships as one portable x64 ZIP. The PowerShell installer, WinGet, Scoop, and `embrasure update` all consume that same archive.

## Release contents

`embrasure-<version>-x86_64-pc-windows-msvc.zip` contains:

- `bin\embrasure.exe`
- `libexec\embrasure\python\sqlglot-*.whl`
- licenses, notices, examples, and documentation

The release workflow also publishes `install.ps1`, `SHA256SUMS`, an SPDX SBOM, GitHub build provenance, and generated WinGet/Scoop manifests. It does not require WiX, Azure, a signing certificate, elevation, services, tasks, shortcuts, or telemetry.

The executable statically links the Microsoft C runtime, so users do not need to install the Visual C++ Redistributable separately. Python and dbt remain project-managed prerequisites.

## Package-manager publication

After the GitHub release exists:

1. Extract `embrasure-<version>-windows-package-manifests.zip`.
2. Publish `scoop/embrasure.json` to `EmbrasureAI/scoop-bucket` as `bucket/embrasure.json`.
3. Submit the three files under `winget/` to `microsoft/winget-pkgs` at `manifests/e/EmbrasureAI/Embrasure/<version>/`.
4. Verify `scoop install embrasure/embrasure` and `winget install --id EmbrasureAI.Embrasure --exact` on clean Windows 11 machines.

The Scoop manifest contains auto-update metadata for the bucket's release bot. WinGet updates require a new manifest submission.

## Release gate

CI covers PowerShell 5.1 and 7 parsing, ZIP traversal and checksum rejection, a current-user install under a Unicode path containing spaces, same-version replacement, rollback, exact PATH cleanup, user-data preservation, PE metadata, SQLGlot discovery, and default/all-feature Rust tests.

Before the first public release, repeat the full install → `embrasure doctor` → update → uninstall journey with the direct script, a standard user, and a real dbt project on clean Windows 11 and Windows Server 2022 VMs. Confirm that no elevation or reboot is requested and retain the installer log. After publishing the Scoop manifest and after the WinGet submission is accepted, test each package-manager install and update path on a clean Windows 11 machine before advertising those commands as available.

The public artifacts are unsigned. SmartScreen or organization policy can still warn about or block direct downloads. SHA-256 checksums, package-manager manifests, and GitHub attestations verify integrity but do not replace Authenticode publisher identity.
