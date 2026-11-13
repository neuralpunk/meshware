# People and access

Who may use the mesh, and what each person may do. Read this before anybody but you uses it.

## How access works

**There are no passwords and no login screen.** A person is a **key** that lives on their own
computer, in `~/.meshware/identity/user.key`. Every request they make is signed with it, and every
machine checks the signature and the person's roles before doing anything.

- **You are the first admin.** Whoever started the mesh with `meshnode up`, or with Director's
  **Create a new mesh**, holds its first admin key.
- **Everyone else asks to be let in**, and an admin decides what they may do.
- **What they may do is a role**, given for the whole mesh or for some machines, and for as long as
  you choose.

```console
$ meshnode whoami           # your key, your name in this mesh, and what you may do
```

## Let someone in

**1. They ask.** On their own computer, with `meshnode` installed and able to reach any machine in
the mesh:

```console
$ meshnode enrol --as mia
Your identity:  mwu1tt5r…  @ laptop-mia
Enrolment code: BRAVE-OTTER-4417
Show this code to a mesh admin. Waiting for approval…
```

In Director, a new person does the same from the welcome screen (**Join with an invite**, or
**Join a mesh on this network**), then gives the name people will know them by.

**2. You check the code.** Their request reaches every machine by itself. Ask them to read out
their code, and compare it with the one you see:

```console
$ meshnode users pending
```

The code is worked out from their key on both sides and never sent, so matching codes mean the
request is really theirs.

**3. You let them in, with a role:**

```console
$ meshnode users approve BRAVE-OTTER-4417 --role operator
```

Without `--role` they get `viewer`, which can look but not change anything. `meshnode users decline`
turns a request away. A request nobody answers expires within the hour.

**In Director:** people waiting appear at the top of **Access**. **Let in…** asks which role to give
them; **Not now** or **Decline…** turns them away.

If you already have someone's key (they can print it with `meshnode whoami`), you can add them
directly: `meshnode users add mia --key mwu1tt5r… --role developer`.

## Roles

| Role | Can |
|---|---|
| **admin** | Everything: letting people in, giving roles, managing machines and secrets |
| **operator** | Keep things running: run work, cancel anybody's work, and manage deployments and schedules |
| **developer** | Ship things: run work, manage deployments and schedules, and publish scripts, services and Git sources |
| **auditor** | Read everything, including the audit log and which secrets exist, and change nothing |
| **viewer** | Read machines, jobs and their history. Not secrets, not the audit log |

Give each person the smallest role that lets them do their job. You can always add more.

**In Director:** *Access › What can each role do?* compares them side by side.

## Choose where, and for how long

A role is given as a **grant**, and a grant can be narrowed. It can apply to some machines only,
chosen by tag or name, and it can expire:

```console
$ meshnode grants add mia --role operator --nodes tag=staging --expires 30d
$ meshnode grants add sam --role developer --services 'web-*'
$ meshnode grants                          # every grant
$ meshnode grants revoke grant-7f2k…       # take one away
```

Grants add up: a second grant joins whatever the person already had. An expired grant leaves the
person in the mesh with nothing to do there, and you can grant again at any time.

`--nodes` takes the same selectors as `run --on`: `any`, a machine's name, or `tag=…`.

**In Director:** *Access › People*, select the person, then **Grant a role**. It asks where (the
whole mesh, some machines by tag, or one machine) and for how long, and shows what the person will
and will not be able to do before you save.

## Terminals on machines

`meshnode shell` opens a terminal on a machine, through the mesh, with no SSH:

```console
$ meshnode shell web-01 --reason "INC-4412 disk full"
```

The reason is required and goes in the audit log. A terminal needs three things: the machine must
accept terminals (`[sessions] accept = "on"` in its configuration), the person must have a role
that runs work on that machine, and a **session policy** must name them:

```console
$ meshnode sessions policy put oncall --users 'ops-*' --nodes tag=web --allow shell --expires-in 8h
$ meshnode sessions                        # who has one open right now
```

A terminal runs as the account the node runs as, and is confined like a job. Recording
terminals is off by default (`[sessions] record`).

## Scripts and pipelines: service accounts

A CI pipeline should not use a person's key. Give it a **service account**: a key of its own, with
a role, a scope, and an expiry it cannot outlive.

```console
$ meshnode serviceaccounts create ci-deploy --role developer --on tag=staging --expires-in 90d
```

The key is written to `ci-deploy.key` on your computer and never stored anywhere else, so keep it
where the pipeline can read it, and point the pipeline at it with `MESHWARE_USER_IDENTITY`. A
service account cannot be an admin. `meshnode serviceaccounts` lists them and how long each has
left.

**In Director:** *Access › Machines* lists service accounts and makes new ones.

## Single sign-on

If your company has an identity provider that speaks OpenID Connect, such as Microsoft Entra ID,
a person can prove who they are with it before asking to join. The admin approving the
request then sees their verified name, email and groups, and a suggested role.
[People, machines and roles](../operating/identity.md#4-sso) sets it up. Single sign-on does not
create or remove people by itself: approving is still an admin's decision.

## When a key is lost or someone leaves

**A lost or stolen key:** replace it, and the person keeps their name and roles:

```console
$ meshnode users rotate mia --new-key mwu1…      # the key from their new `meshnode whoami`
```

**Someone leaves:** revoke them. It is permanent: that key can never be let in again.

```console
$ meshnode users revoke mia --yes
```

**In Director:** *Access › People*, select the person: **Replace key…**, or revoke them.

## Protect your own key

Your key is your access. Two ways to keep it safe:

- **A passphrase:** `meshnode identity protect`. You unlock it once per login with
  `meshnode identity unlock`, and Director asks for it when it starts.
- **A security key:** `meshnode identity hardware` keeps the key on a YubiKey or similar, through
  an ssh-agent, so there is nothing on the disk to copy.

**In Director:** *Settings › This machine › Security key* › **Set up a security key** walks
through it: the two commands that put a key on a YubiKey, connecting it with its PIN, and checking
it signs before anything changes. Then it says what the switch takes: the founder's key joins as a
second admin (a founding key can never be replaced, so it is kept as your spare, your way back
in), and any other admin's account moves onto the key with its roles. **Go back to your previous
key** undoes it. Key files in `~/.ssh` are not offered — they would protect nothing.

Optionally, publish the proof the device made for the key, and *Access › People* shows it as held
on a security key. An admin chooses whose proofs the mesh believes in *Access › Machines ›
Security key makers*.

**Keep more than one admin.** An admin key that is lost with nobody else to replace it cannot be
recovered. Add a second admin, and [back up](../operating/backup.md) your key.

## Who did what

Every change — people let in, roles given, jobs run, secrets set, terminals opened — is signed and
recorded in the audit log, on every machine:

```console
$ meshnode audit                          # newest first
$ meshnode audit --user mia --since 7d
$ meshnode audit --action job.run --limit 20
```

Only admins and auditors can read it. **In Director:** *Access › History*.

## Go deeper

- [People, machines and roles](../operating/identity.md): how each part works, in detail.
- [Security](../operating/security.md): what a key protects, and what a compromised machine can and
  cannot do.
- [Secrets](secrets.md): passwords and tokens for jobs, and who may use them.
