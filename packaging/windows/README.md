# Windows release

Windows ships as one portable x64 ZIP. The PowerShell installer, WinGet, Scoop, and `embrasure update` all consume that same archive.

## Release contents

`embrasure-<version>-x86_64-pc-windows-msvc.zip` contains:

- `bin\embrasure.exe`
- `libexec\embrasure\python\sqlglot-*.whl`
- licenses, notices, examples, and documentation

The release workflow also publishes `install.ps1`, `SHA256SUMS`, an SPDX SBOM, GitHub build provenance, and generated WinGet/Scoop manifests. It does not require WiX, Azure, a signing certificate, elevation, services, tasks, shortcuts, or telemetry.

## Package-manager publication

After the GitHub release exists:

1. Extract `embrasure-<version>-windows-package-manifests.zip`.
2. Submit `scoop/embrasure.json` to `ScoopInstaller/Main`.
3. Submit the three files under `winget/` to `microsoft/winget-pkgs` at `manifests/e/EmbrasureAI/Embrasure/<version>/`.
4. Verify `scoop install embrasure` and `winget install --id EmbrasureAI.Embrasure --exact` on clean Windows 11 machines.

The templates contain Scoop auto-update metadata, so its release bot can update accepted versions. WinGet updates require a new manifest submission.

## Release gate

CI covers PowerShell 5.1 and 7 parsing, ZIP traversal and checksum rejection, a current-user install under a Unicode path containing spaces, same-version replacement, rollback, exact PATH cleanup, user-data preservation, PE metadata, SQLGlot discovery, and default/all-feature Rust tests.

Before the first public release, repeat the full install → `embrasure doctor` → update → uninstall journey as a standard user on clean Windows 11 and Windows Server 2022 VMs with a real dbt project. Test the direct script, WinGet, and Scoop paths. Confirm that no elevation or reboot is requested and retain the installer log.

The public artifacts are unsigned. SmartScreen or organization policy can still warn about or block direct downloads. SHA-256 checksums, package-manager manifests, and GitHub attestations verify integrity but do not replace Authenticode publisher identity.
