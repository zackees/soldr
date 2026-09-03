# One-time macOS guest bootstrap

Everything else is automated. dockur/macos has no unattended install path
(confirmed against its docs), and Docker-OSX's prebuilt `:auto` tag no longer
exists on Docker Hub -- only `latest`/`master`. So the install must be driven
once by hand; after that this is fully scriptable through
`ci/macos_x64_guest.py` (soldr#3071).

## Step 1 -- install macOS (~30-60 min, one core)

Open http://localhost:8006

1. Disk Utility -> select the ~128 GB QEMU HARDDISK -> Erase -> APFS -> Erase
2. Quit Disk Utility -> "Reinstall macOS Ventura" -> Continue -> pick that disk
3. Walk the setup screens. Skip Apple ID. Create a local account.
   **Use username `runner`** (the scripts assume it; override with GUEST_USER).
   Pick any password you'll remember.

## Step 2 -- enable SSH (in the guest)

System Settings -> General -> Sharing -> **Remote Login: on**

Then confirm the guest's IP inside the guest (Terminal):

    ipconfig getifaddr en0

## Step 3 -- expose SSH to the host

dockur maps only :8006 by default. Recreate the container with a port map,
keeping the SAME storage volume so the install is preserved:

    docker rm -f soldr-macos-x86
    docker run -d --name soldr-macos-x86 \
      --device=/dev/kvm --device=/dev/net/tun --cap-add NET_ADMIN \
      -p 8006:8006 -p 2222:22 \
      -e VERSION=ventura -e RAM_SIZE=8G -e CPU_CORES=1 -e DISK_SIZE=128G \
      -v ~/.clud/docker-mac-x86/storage:/storage \
      dockurr/macos

## Step 4 -- install cargo-nextest and rsync in the guest

From the host (`cargo-nextest` here is a universal Mach-O, so it runs on
Intel):

    scp -P 2222 ~/.clud/docker-mac-x86/cargo-nextest runner@localhost:~/
    ssh -p 2222 runner@localhost 'sudo mv ~/cargo-nextest /usr/local/bin/ && sudo chmod +x /usr/local/bin/cargo-nextest'

`ci/macos_x64_guest.py sync-in`/`sync-out` prefer `rsync`; Ventura ships it,
but if a leaner base image does not, `sync-in`/`sync-out` fall back to
`scp -r` automatically.

No Rust toolchain, no Xcode CLT, no Homebrew, and no Python needed in the
guest -- the test binaries and release archives are entirely prebuilt on
Linux; the guest only ever executes what CI ships it.

## Step 5 -- add the CI ssh key

Generate a dedicated keypair (not your personal one), add the public half to
the guest's `~runner/.ssh/authorized_keys`, and add the private half to the
repo as the `SOLDR_MACOS_GUEST_SSH_KEY` secret -- `ci/macos_x64_guest.py`
writes it to `$RUNNER_TEMP/guest_key` (mode 600) and uses it as the ssh
identity for every guest command.

    ssh-keygen -t ed25519 -f soldr-macos-guest-key -N ""
    ssh -p 2222 runner@localhost 'mkdir -p ~/.ssh && chmod 700 ~/.ssh'
    scp -P 2222 soldr-macos-guest-key.pub runner@localhost:~/.ssh/authorized_keys_new
    ssh -p 2222 runner@localhost 'cat ~/.ssh/authorized_keys_new >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && rm ~/.ssh/authorized_keys_new'
    gh secret set SOLDR_MACOS_GUEST_SSH_KEY --repo zackees/soldr < soldr-macos-guest-key

## Step 6 -- snapshot, so this never has to happen again

    docker stop soldr-macos-x86
    tar -I 'zstd -T0' -cf ~/.clud/docker-mac-x86/macos-ready.tar.zst \
      -C ~/.clud/docker-mac-x86 storage
    docker start soldr-macos-x86

Restoring that tarball rebuilds a ready guest in seconds.

## Step 7 -- bake and publish

    ./bake.sh   # builds Dockerfile.guest from the prepared storage/ and
                # pushes ghcr.io/zackees/soldr/macos-x64-guest:ventura

## Then the loop is fully automated

CI (`.github/workflows/_ci-target-run.yml`, `.github/workflows/release-auto.yml`)
drives the published image entirely through `ci/macos_x64_guest.py`:

    python ci/macos_x64_guest.py preflight            # KVM + docker sanity
    python ci/macos_x64_guest.py start --image ghcr.io/zackees/soldr/macos-x64-guest:ventura
    python ci/macos_x64_guest.py sync-in --src <host dir> --dest <guest dir>
    python ci/macos_x64_guest.py exec -- <argv>        # exact exit code propagates
    python ci/macos_x64_guest.py sync-out --src <guest dir> --dest <host dir>
    python ci/macos_x64_guest.py stop
