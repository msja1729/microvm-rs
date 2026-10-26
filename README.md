# microvm-rs

**A tiny computer that runs inside your computer.**

microvm-rs is a small program, written in Rust, that creates a *virtual machine*: a pretend
computer with its own CPUs, memory, disk and network. Inside it, a real Linux system starts
up in a few seconds, and you can type commands into it just like on a normal computer.

It works the same way as [Firecracker](https://firecracker-microvm.github.io/), the tool
Amazon uses to run AWS Lambda, but it is much smaller and built for learning.

<p align="center">
  <a href="https://github.com/msja1729/microvm-rs/releases/latest">
    <img src="https://img.shields.io/github/v/release/msja1729/microvm-rs?style=for-the-badge&label=%E2%AC%87%20Download&color=2ea44f" alt="Download the latest release">
  </a>
</p>

> [!IMPORTANT]
> ### ⬇️ Want to try it? Download it from the Releases page
>
> You **do not** need to build anything. A ready-to-run package is on the
> **[Releases page](https://github.com/msja1729/microvm-rs/releases/latest)**.
>
> Download the file named **`microvm-rs-<version>-x86_64-linux.tar.gz`**, then follow the
> [install steps below](#-install-and-run).

---

## ✅ What you need

- A computer running **Linux** on a normal 64-bit Intel or AMD processor
  (Windows users: **WSL2** works).
- **KVM** turned on. This is the part of Linux that runs virtual machines. Check it with:

  ```bash
  ls -l /dev/kvm
  ```

  If you see `No such file or directory`, turn on virtualization in your BIOS/UEFI settings
  (on WSL2, turn on nested virtualization).
- Permission to use KVM. Run this once, then **log out and log back in**:

  ```bash
  sudo usermod -aG kvm $USER
  ```

You do **not** need to be root to run the virtual machine itself.

## 🚀 Install and run

**Step 1.** Go to the **[Releases page](https://github.com/msja1729/microvm-rs/releases/latest)**
and download `microvm-rs-<version>-x86_64-linux.tar.gz`.

**Step 2.** Open a terminal in the folder where you saved it, and unpack it:

```bash
tar xzf microvm-rs-*-x86_64-linux.tar.gz
cd microvm-rs-*-x86_64-linux
```

**Step 3.** Start the virtual machine:

```bash
./run.sh
```

After a few seconds you will see a prompt like this. You are now **inside** the virtual
machine:

```
phase1#
```

**Step 4.** Try some commands:

```bash
nproc                     # how many CPUs the virtual machine has
uname -r                  # which Linux version it runs
mkdir -p /mnt && mount /dev/vda /mnt && cat /mnt/hello.txt    # read its virtual disk
```

**Step 5.** To turn the virtual machine off, type:

```bash
reboot -f
```

### More ways to run it

| What you want | Command |
|---|---|
| Use 4 CPUs instead of 2 | `VCPUS=4 ./run.sh` |
| Make the disk read-only | `./run.sh --disk-ro` |
| Give it more memory (1 GB) | `./run.sh --mem-mib 1024` |
| See every option | `./vmm boot --help` |

<details>
<summary><b>Connecting the virtual machine to the network (optional)</b></summary>

This needs a one-time setup with `sudo` each time your computer starts:

```bash
sudo ip tuntap add dev vmtap0 mode tap user "$USER"
sudo ip addr add 172.16.0.1/24 dev vmtap0
sudo ip link set vmtap0 up
```

Then start the virtual machine with the network attached:

```bash
./run.sh --tap vmtap0
```

And inside the virtual machine:

```bash
ip addr add 172.16.0.2/24 dev eth0
ip link set eth0 up
ping -c 3 172.16.0.1
```

</details>

### What is in the download?

| File | What it is |
|---|---|
| `run.sh` | The easy way to start everything |
| `vmm` | The virtual machine program itself |
| `bzImage` | The Linux kernel that runs inside |
| `initramfs.cpio` | A tiny set of Linux tools (BusyBox) |
| `disk.ext4` | A 64 MB virtual hard disk |
| `hello16.bin`, `hello64.bin` | Two tiny test programs (`./vmm raw --image hello64.bin --mode long`) |

📖 **Want a full guide?** Read the **[User Manual (PDF)](docs/user-manual.pdf)**. It walks
through every command step by step.

## 🛠️ Having trouble?

| Problem | Fix |
|---|---|
| `/dev/kvm is not accessible` | Run `sudo usermod -aG kvm $USER`, then log out and back in |
| `/dev/kvm: No such file or directory` | Turn on virtualization (VT-x / AMD-V) in your BIOS, or nested virtualization in WSL2 |
| `mount: ... failed` | Run `mkdir -p /mnt` first |
| The program says `GLIBC ... not found` | Your Linux is too old for the ready-made program. Use Ubuntu 24.04 or newer, or build it yourself (see below) |

## ✨ What it can do

- **Starts real Linux** (version 6.12) in about 4 seconds.
- **Virtual hard disk**: files you save inside stay saved after you turn it off.
- **Virtual network card**: the virtual machine can talk to your computer.
- **Up to 4 CPUs**.
- **Safe by design**: each part of the program is only allowed to do the few things it needs
  (this uses a Linux feature called *seccomp*). Even if something inside the virtual machine
  misbehaves, it cannot crash the program.
- **Checked by tools**: the code that reads data from the virtual machine is checked with
  mathematical proofs ([Kani](https://github.com/model-checking/kani)) and random testing
  (fuzzing).

---

## For developers

<details>
<summary><b>Build it yourself from the source code</b></summary>

```bash
# Tools needed (one time)
sudo apt install build-essential nasm flex bison bc libelf-dev libssl-dev xz-utils curl \
                 busybox-static cpio e2fsprogs

cargo build --release           # the vmm program  -> target/release/vmm
scripts/build-guests.sh         # tiny test programs
scripts/build-kernel.sh         # Linux kernel (slow: takes a while)
scripts/build-initramfs.sh      # BusyBox tools
scripts/build-disk.sh           # virtual disk
scripts/package-release.sh      # makes the same download as the Releases page, in dist/
```

</details>

<details>
<summary><b>Run the tests</b></summary>

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                      # quick tests
cargo test -- --ignored         # tests that boot Linux
scripts/verify-phase-1.sh       # checks every Phase 1 goal, prints PASS/FAIL
sudo scripts/setup-tap.sh       # needed once for the network tests
scripts/verify-phase-2.sh       # checks every Phase 2 goal
```

</details>

<details>
<summary><b>How the code is organised</b></summary>

```
src/arch/x86_64/   sets up the virtual CPU so Linux can start
src/devices/       the pretend devices: serial console, disk, network (virtio)
src/vm.rs ...      creating the virtual machine, running CPUs, security rules
tests/             automatic tests
fuzz/              random-input testing
specs/  notes/     the plan for each phase and what was learned
docs/              user manual
study-notes/       beginner notes on the ideas behind the project
```

</details>

### Project progress

| Phase | Goal | Status |
|---|---|---|
| 1 | Start Linux and type commands into it | ✅ Done |
| 2 | Disk, network, multiple CPUs, security, proofs | ✅ Done |
| 3 | Encrypted virtual machines (needs special hardware) | ⏳ Not started |

## License

The code is under the **[MIT License](LICENSE)**: you are free to use, copy and change it.
The download also includes Linux and BusyBox, which use their own license (GPL-2.0).
