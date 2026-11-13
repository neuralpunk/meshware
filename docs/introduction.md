# What Meshware is

Meshware turns the Linux machines you already have into one computer. Put `meshnode` on each of
them, and they pool their CPUs, memory and GPUs: you can run a command on all of them at once, keep
a service running across them, or spread a big job over whatever is free.

There is no server to set up first. Every machine is equal, holds a copy of everything the mesh
knows, and keeps working if the others go away. That is what "no control plane" means, and it is
why installing Meshware is installing one file.

## The words you will meet

| Word | What it means |
|---|---|
| **Machine**, or **node** | A computer running `meshnode`. Each one runs work, and each one can answer for the whole mesh |
| **Mesh** | Every machine that has joined together. A machine belongs to one mesh |
| **Job** | One command run once, on one machine or on many. It ends, and its output and result are kept |
| **Deployment** | Something that should *keep* running: a web server, a queue worker. Meshware starts the copies you asked for and replaces any that stop |
| **Schedule** | A job that runs itself on a clock: every hour, or at 03:00 |
| **Person** | Somebody allowed to use the mesh. A person is a key on their own computer, not a password |
| **Role** | What a person may do: `admin`, `operator`, `developer`, `auditor` or `viewer` |
| **Secret** | A password or token a job needs, stored encrypted and handed only to the job that uses it |
| **Tag** | A label on a machine, like `gpu` or `dc=east`, used to choose where work goes |
| **Director** | The desktop app. It does everything the command line does, and shows the mesh live |

## How the pieces fit

```
   your laptop                      the mesh
 ┌───────────────┐          ┌──────────┐  ┌──────────┐  ┌──────────┐
 │ Director  or  │  signed  │  node    │──│  node    │──│  node    │
 │ meshnode CLI  │ ───────▶ │  web-01  │  │  gpu-01  │  │  db-01   │
 │ (your key)    │ request  └──────────┘  └──────────┘  └──────────┘
 └───────────────┘               every node holds the whole picture
```

You ask one machine, the one you are on or any other, and it decides where the work should run
and sends it there. Every request carries your signature, and every machine checks it against
the roles you have been given. A machine that is down is simply not chosen.

## What you can do with it

- **Run things across machines.** One command on one machine, on every machine tagged `gpu`, or
  on all of them, with the output streaming back live. [Running work](guides/running-work.md)
- **Keep services up.** Describe a service once, and Meshware keeps the copies running, rolls out
  changes carefully and moves copies off machines that fail. [Deployments](deployments.md)
- **Let people in, with the right access.** Each person gets a role, for the whole mesh or for
  some machines, for as long as you choose. [People and access](guides/people-and-access.md)
- **Keep secrets out of scripts.** Passwords and tokens are sealed and only opened by the job
  that needs them. [Secrets](guides/secrets.md)
- **Build faster.** Put `meshnode cc` in front of your compiler and a build spreads across the
  mesh — and never compiles the same file twice. [Compiling](guides/compiling.md)
- **See everything.** Director shows every machine, job, deployment and person, live.
  [The Director guide](director.md)

## Where to start

1. **[The quickstart](quickstart.md)** takes you from nothing to two machines running your
   commands, in about fifteen minutes.
2. **[Adding and managing machines](guides/machines.md)** when you want more of them.
3. **[Deployments](deployments.md)** when you want something to stay running.
4. **[People and access](guides/people-and-access.md)** before anybody else uses the mesh.

Meshware runs on Linux, and so does Director. [Platforms](operating/platforms.md) has the details.
