# CRIO Integration

This document will describe how to get the CRIO integration of BPFContain running

# SETUP

Following the [CRIO installation tutorial](https://github.com/cri-o/cri-o/blob/main/install.md), build CRIO and PINNS binaries from source with debug symbols and copy/move both binaries to /usr/local/bin. To build the CRIO binary, use the command: 

```console
user@system:~/{CRIO_REPO}$ go build -trimpath -gcflags '-N -l' -ldflags='-compressdwarf=false  -X github.com/cri-o/cri-o/internal/pkg/criocli.DefaultsPath="" -X github.com/cri-o/cri-o/internal/version.buildDate='2022-02-28T05:06:37Z' -X github.com/cri-o/cri-o/internal/version.gitCommit=731eb6e8f48f28373606e019a7b2a8425aa882ab -X github.com/cri-o/cri-o/internal/version.gitTreeState=dirty ' -tags "containers_image_ostree_stub     libdm_no_deferred_remove containers_image_openpgp seccomp selinux " -o bin/crio github.com/cri-o/cri-o/cmd/crio
```

This effect can also be achieved by modifying the Makefile.

Create a file at/etc/crio/crio.conf.d/ named 01-crio-runc.conf containing the following:

```yaml
[crio.runtime.runtimes.runc]
runtime_path = "/usr/bin/runc"
runtime_type = "oci"
runtime_root = "/run/runc"
```

Copy the crio.service file located at crio/contrib/systemd to /usr/lib/systemd/system

To create pods and containers with CRIO, the crictl tool must be built located at [https://github.com/kubernetes-sigs/cri-tools](https://github.com/kubernetes-sigs/cri-tools).

Build the crictl binary and copy/move it into /usr/local/bin.

Tutorials on using the crictl command are found [here](https://github.com/cri-o/cri-o/blob/main/tutorials/crictl.md) and [here](https://github.com/kubernetes-sigs/cri-tools/blob/master/docs/crictl.md)





