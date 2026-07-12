# Privileged features

Ordinary Hephaestus development is root-free and uses an ad-hoc signature with
`com.apple.security.virtualization`. Two development areas require additional
host authorization.

## Bridged vmnet networking

Transparent link-local MMDS for a stock image requires a network path where the
host can answer on the guest's L2. The planned path uses a vmnet-backed VZ
attachment and the restricted `com.apple.vm.networking` entitlement.

Under System Integrity Protection, an ad-hoc signature cannot grant that
entitlement. Development requires:

1. An Apple Development identity.
2. An App ID with the Virtualization Networking capability approved.
3. A Development provisioning profile carrying that capability.
4. A build signed with `hephaestus-vmnet.entitlements`.

Probe the current host before working on this path:

```console
$ just probe-vmnet
```

A printed interface list means the entitlement was honored. Termination such as
`Killed: 9` with no output usually means AMFI rejected the entitlement or the
profile is absent. Build with the authorized identity using:

```console
$ HEPHAESTUS_SIGN_IDENTITY='<identity or hash>' just sign-vmnet
```

The normal build remains on base entitlements. Do not recommend disabling SIP
or AMFI as a development setup.

## Cross-user jailer tests

Meaningful uid/gid isolation would require starting a supervisor as root and
dropping to a dedicated service account. That feature is not currently
implemented. Resource-limit and sandbox-profile tests are root-free:

```console
$ just jailer-rlimit-check
$ just fc-compat-sandbox-config
```

If cross-user isolation is added, its tests must create or select a non-login
service user explicitly, verify the daemon's effective uid/gid, and avoid making
sudo a requirement for the ordinary test suite.

## Keep host facts local

Signing identities, usernames, installed profiles, and one machine's current
SIP or sudo state are local diagnostics, not project requirements. Do not commit
them to shared documentation or scripts.
