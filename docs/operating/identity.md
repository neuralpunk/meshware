# People, machines and roles

*Reference. Describes what is in the tree.*

**There is no login screen, and that is the design rather than an omission.** A person is a key
on their own machine, every request is signed with it, and roles attach to the key.

---

## 1. Getting a person into a mesh

```console
# on the new person's machine:
$ meshnode enrol --as mia
Your identity:  mwu1tt5r…  @ laptop-mia
Enrolment code: BRAVE-OTTER-4417
Show this code to a mesh admin. Waiting for approval…

# on any admin's machine — the request arrived on its own:
$ meshnode users pending
$ meshnode users approve BRAVE-OTTER-4417 --role operator
```

The code is a display encoding of the public key plus a nonce, derived identically on both
sides and **never transmitted** — so "read me the code on your screen" is a genuine
out-of-band confirmation. A request grants nothing: it is a row in a queue that expires within
the hour.

## 2. Roles and grants

Five built-in roles. Custom roles are not built.

| Role | Can |
|---|---|
| `admin` | Everything, including granting roles |
| `operator` | Run and cancel jobs, drain nodes |
| `developer` | Manage services, content and contracts; run |
| `auditor` | Read everything **including the audit log**; run nothing |
| `viewer` | Read except secrets and audit |

A **grant** is `{user, role, node selector, service glob, expiry}`, admin-signed and replicated.
Scoping is real: a grant can be narrowed to `tag=staging` and to `deploy-*`, and it can expire.

```console
$ meshnode grants add mia --role operator --nodes tag=staging --expires 30d
$ meshnode grants ls
$ meshnode whoami                    # what am I, and what may I do about it
```

Authorization is evaluated **on the node serving or executing the request**, against its own
materialized copy of state. There is no policy server to be down.

## 3. Service accounts — a pipeline is not a person

**A CI job should not authenticate as a human with a copy of their key file.** Machine
principals are the answer:

```console
$ meshnode serviceaccounts create ci-deploy \
      --role developer --on tag=staging --expires-in 90d
service account 'ci-deploy' created
  identity   mwu1ktdz…
  role       developer on tag=staging
  expires    2026-12-05 20:02
  key        ci-deploy.key  (mode 0600)
```

The key is minted on your machine, written to a file, and **never stored by meshware** — it
cannot be shown to you again. Put it where the pipeline reads it and set
`MESHWARE_USER_IDENTITY` to that path.

**Two clocks, and they differ.** A *grant* expiring leaves a live key with no roles, which an
admin can re-grant — right for a person's on-call window. An *account* expiring is terminal for
that credential: re-granting does nothing, because the principal itself is out of time. That is
the one you want on a CI token, which is why `--expires-in` is required and `--never-expires`
has to be typed.

**A service account may not hold `admin`**, refused at the API and at merge. An admin key can
mint users and grants, so a leaked pipeline credential holding one is permanent mesh
compromise.

They appear in the audit log marked as machines:

```console
$ meshnode audit --limit 2
8s ago    ci-deploy*   job.run     job-agqh…   ok   echo deploy on web-01
* a service account — a machine principal, not a person
```

Revoke one the way you revoke a person: `meshnode users revoke ci-deploy --yes`.

## 4. SSO

meshware can verify an identity provider your company already runs. It does **not** add a
login: it adds *evidence* for the admin who approves an enrolment.

```console
# an admin, once:
$ meshnode oidc providers put entra \
      --issuer https://login.microsoftonline.com/<tid>/v2.0 \
      --client-id <app-id> \
      --jwks-uri https://login.microsoftonline.com/<tid>/discovery/v2.0/keys \
      --device-endpoint https://login.microsoftonline.com/<tid>/oauth2/v2.0/devicecode \
      --token-endpoint https://login.microsoftonline.com/<tid>/oauth2/v2.0/token \
      --group-role 'platform=operator'

# a person, before enrolling:
$ meshnode login entra
$ meshnode enrol --as mia
```

The admin then sees who they are actually approving:

```console
$ meshnode users pending
mia   BRAVE-OTTER-4417   laptop-mia   2m ago   mwu1tt5r
      ✓ Mia <mia@example.com> — verified by entra via alpha
        groups: platform
        suggests --role operator
```

**Device-code flow, so nothing listens on the machine the person is at** — which also makes it
the only flow that works over SSH on a headless box. The **node** does the verification, against
the provider record an admin signed and keys it fetched itself; the OIDC nonce binds the token
to that specific meshware key, so a token issued for somebody else cannot be replayed onto it.

**Your identity provider being down does not affect the mesh.** Nothing on the request path
reads a provider record: existing keys keep working, jobs keep running, and the only thing that
stops is new people enrolling *through SSO* — who can still enrol the way they always could.

**What SSO does not do yet:** it does not create users automatically, and it does not
deprovision them. Making a verified login *become* a user means authoring a `UserAdd`, which is
only accepted with an **admin's signature** — and a node has no admin key, which is exactly why
a compromised node cannot escalate. Closing that gap is an authority-model change and is
deliberately not done. Approval remains an admin's signature.

## 5. Interactive access

```console
$ meshnode shell web-01 --reason "INC-4412 disk full"
```

A real terminal over the mesh's own transport — no sshd, no key distribution, no second account
system. **`--reason` is required**, covered by your signature, and in the audit log at both
open and close.

Who may open one is its own policy, separate from the run grant:

```console
$ meshnode sessions policy put oncall --users 'ops-*' --nodes any --allow shell --expires-in 8h
$ meshnode sessions                 # who is holding one right now
$ meshnode sessions close <id>
```

A terminal needs **both** a runner grant on the node and a policy naming the user and the node.
Nobody gains a prompt from a policy alone.

Optional local transcripts with `[sessions] record`, off by default — recording a human's
terminal is an organisation's decision, not a default.

**Not built:** a second person's approval before a terminal opens, and just-in-time elevation.
A session policy decides who may open one; nothing asks anybody else at the moment they do.

## 6. Revocation

```console
$ meshnode users revoke mia --yes         # terminal: the key can never be re-enrolled
$ meshnode grants revoke <grant-id>       # narrower: takes a role away
$ meshnode nodes revoke web-01 --yes      # expels a machine, irreversible
```

Revocation is a signed op like everything else, so it propagates by the same anti-entropy that
carries a job record and applies on merge. Across a partition it applies when the link returns —
measured at **~51 ms after a heal**, and the window is visible in the audit log.

For a node you *suspect* but do not want to expel, quarantine is the reversible option — it
stops work **and terminals**, unlike `nodes contribute off`:

```console
$ meshnode nodes quarantine web-07 --reason "unexpected outbound to 5.6.7.8"
$ meshnode nodes quarantine web-07 --lift
```

For a person whose key is lost or suspect, rotation keeps the person and replaces the key,
carrying their roles across:

```console
$ meshnode users rotate mia --new-key mwu1…      # from their `meshnode whoami`
```

Both are in Director too — *Mesh › Fleet* → the machine → **Quarantine**, and *Access › People* → the
person → **Replace key…**
