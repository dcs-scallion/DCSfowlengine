$env:RUST_LOG="info"
Set-Location $PSScriptRoot
../../../../bftools/bftools.exe miz --output ./Caucasus1987.miz --base ./base.miz --weapon ./weapon.miz --warehouse ./warehouse.miz

# This line prevents the window from closing automatically
Read-Host -Prompt "Press Enter to exit"