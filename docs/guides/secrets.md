# Secrets

Passwords, tokens and keys that jobs need, and that nobody should see in a script, a log or a
command line. A secret is encrypted on the computer you type it on, and opened only on the machine
that runs the job that uses it, at the moment it starts.

## Set a secret

Only admins can set, change or remove secrets.

```console
$ meshnode secrets set db-pass                        # type the value, then Ctrl-D
$ meshnode secrets set tls-key --from-file ./server.key
$ meshnode secrets ls                                 # names and who can use them, never values
```

Reading the value from the keyboard or a file keeps it out of your shell history. The value is
encrypted before it leaves your computer; no machine stores it unencrypted, and nothing logs it.

**In Director:** *Secrets* › **Set a secret**. **Rotate** sets a new value under the same name, and
**Remove** deletes it. No role can read a value back through Director.

## Use it in a job

Refer to a secret by name in an **environment variable**:

```console
$ meshnode run './backup.sh' --env 'PGPASSWORD={{secret:db-pass}}'
```

The machine that runs the job opens the secret and puts the value in the variable. If the job
prints it, the output shows `[secret:db-pass]` instead of the value (for values of four
characters or more). That catches accidents, not intent: a job that encodes the value before
printing it gets past it.

To give a job the value as a **file** instead, for a key or a certificate, use `secret_file`. The
variable then holds the path of a file only that job can read:

```console
$ meshnode run 'curl --cert "$CERT" https://internal.example' --env 'CERT={{secret_file:tls-key}}'
```

A secret is never allowed in the command itself, only in the environment, because any user on a
machine can read another process's command line.

## Use it in a deployment

The same reference works in a deployment's environment:

```yaml
deployment:
  name: api
  container:
    image: registry.example/api:2.1
    env:
      DATABASE_URL: "postgres://api@db-01/api"
      DATABASE_PASSWORD: "{{secret:db-pass}}"
```

Each copy gets the value when it starts, on whichever machine it lands.

## Decide who can use it

Anyone who can run a job on a machine that holds a secret can make that job print it. So narrow
each secret to the machines and the people that need it:

```console
$ meshnode secrets set db-pass --on tag=db-clients --role developer
$ meshnode secrets scope db-pass --on tag=db-clients --user ci-deploy --user mia
$ meshnode secrets scope db-pass --clear              # back to the default
```

- **`--on`** — only machines matching this selector receive it. Others cannot open it, even if
  they held it before.
- **`--user` and `--role`** — only these people's jobs may use it.

Without a scope, a secret goes to every machine that runs work, but not to machines that run
no work, or quarantined machines.

> [!IMPORTANT]
> Narrowing a scope stops new machines receiving the secret, but cannot take it back from machines
> that already had it. After narrowing, **set the value again** so the old one is useless.

## Rotate and remove

```console
$ meshnode secrets set db-pass            # the same name, a new value: every machine gets it
$ meshnode secrets rm db-pass --yes
```

Change the value where it is used first (the database password, the API token), then set the new
value here. Jobs that start afterwards get it.

## Secrets kept somewhere else

If your secrets already live in HashiCorp Vault or another store, a machine can fetch them from
there when a job starts, with a small program called a provider. The reference is then
`{{secret:vault:apps/web#token}}`: the provider's name, then the path it understands.
[Plugin contracts](../reference/plugins.md#secrets-provider) shows how to set one up, and
`meshnode secrets providers` lists what a machine has.

## What a secret protects against, and what it does not

- **Protected:** the value never appears in the mesh's shared records, in logs, in backups of the
  mesh's state, or on any machine outside its scope.
- **Not protected:** a job that uses a secret can read it, and so can whoever wrote the job. Keep
  the people and machines that can use each secret as few as you can.

[Security](../operating/security.md#4-secrets) explains the encryption in detail.
