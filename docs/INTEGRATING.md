# Adding a game to the passthrough link

This guide is for an AI coding agent, or a person, who has one game and wants it to take part in a
rewrite-bridge passthrough: two programs on one machine, each running its own game, sharing one world. It
covers the three things you can build, the library for your language, a minimal loop for each, how to test
with no second game, and what is not proven.

- **Contract.** [`CONTRACT.md`](CONTRACT.md) is the protocol: v1, revision 6. This guide does not restate it
  and links to the section that decides each point. If the contract and this page disagree, the contract wins.
- **Checked against** rewrite-bridge `main` at `1bc3b02` and the `main` branches of link-c, link-cs and link-java
  (all revision 6), on macOS 26 (arm64) with Rust 1.99.1, Apple clang 21, .NET SDK 8.0.426 and JDK 25 (Java code
  compiled with `--release 8`). Every command and snippet below was run there, except where a section says it was not. Nothing was run on Linux or
  Windows.

## Decide what you are building

| You are building | Meaning | What you write | Where to start |
|---|---|---|---|
| **Host** | A game whose world others appear in | Send the camera every rendered frame and collision near the player; read the guest's poses and frames; draw them with a depth test; own the input focus | [Minimal loops](#minimal-loops), then contract section 9.7 |
| **Guest** | A game or rewrite shown inside a host | Read the host's camera; send poses for your entities; optionally render with that camera and send frames with depth | [Minimal loops](#minimal-loops) |
| **Provider** | Assets loaded from a game's files into a Rust rewrite | A type that implements `Provider`; there is no link and no second process | [`WRITING_A_PROVIDER.md`](WRITING_A_PROVIDER.md) |

A passthrough with two real games needs one host and one guest. You build the role your game plays; the other
role can be a reference program while you work (see [Test with no second game](#test-with-no-second-game)) but
it is not done until a real second game takes part (see [Done means both games take part](#done-means-both-games-take-part)).

## Pick the library

Choose by the language your game's mod or rewrite is written in. All four speak the same protocol and are
tested against each other.

### Rust rewrite or Bevy game

- **Library:** the `rewrite-bridge` crate with its `link` feature. In `Cargo.toml`:
  `rewrite-bridge = { version = "0.3", features = ["link"] }`. The token (revision 6) exists from 0.3.0 only.
- **Bevy 0.20:** `rewrite-bridge-bevy` loads provider output; with its `compose` feature `HostLinkPlugin` and
  `GuestComposePlugin` make a Bevy camera a host that depth-composites a guest's frames. Run against
  `fake_guest --frames` here (see [Bevy host](#bevy-host)). Details: [its README](../rewrite-bridge-bevy/README.md).
- **One copy per build.** Every provider crate (`wt-unpack`, `wf-unpack`, `hytale-unpack`, `nocturne-unpack`,
  `mc-unpack`) and `bridge-guest` and `bridge-testbed` depend on `rewrite-bridge` 0.2.x, and before 1.0 a new
  minor number is a different crate. A build that uses both a 0.2 provider and 0.3 types has two copies and
  fails to compile (see [the README](../README.md#one-copy-for-the-whole-build)). Today that means: with a 0.2
  provider you have the link without the token; with the token you cannot yet use those providers in the same
  build.

### C or C++ mod: Skyrim (SKSE), Fallout 4 (F4SE), GTA V (ScriptHookV), Unreal

- **Library:** [link-c](https://github.com/patodois/link-c), one header `rewrite_link.h`, C99, compiles as C++.
- **Untested in these frameworks.** Its README lists SKSE, F4SE, ScriptHookV and Unreal modules as the intended
  users and says it was not built or run inside any of them. It was run as a plain program on macOS only. Its
  Windows code is written against documentation, not run. 32-bit x86 is not a target.

### C# mod: Unity with BepInEx or MelonLoader, Valheim, Unturned

- **Library:** [link-cs](https://github.com/patodois/link-cs), `netstandard2.0`, no dependencies.
- **Untested in these frameworks.** Its README says nothing was run inside BepInEx or MelonLoader, and that
  Unity's own runtimes were not checked for `MemoryMappedFile` (needed for frames; poses, cameras and
  collision do not use it). It was run as a .NET 8 console program on macOS only.

### Java: Minecraft on Fabric, Forge, NeoForge and Quilt; Hytale plugins; Java server plugins

- **Library:** [link-java](https://github.com/patodois/link-java), Java 8 or newer, no dependencies.
- **Minecraft:** [minecraft-link](https://github.com/patodois/minecraft-link) is a client mod that bundles
  link-java and acts as host or guest. Per its README it is built and tested on Fabric (and Quilt with the same
  jar), NeoForge and Forge for Minecraft 26.3 and 1.20.1, Forge for 1.12.2 and 1.7.10. Read it as the worked
  example of a Java mod.
- **Hytale plugin:** the link-java README names a Hytale plugin as a use (the code was written inside one); no
  Hytale plugin is in these repositories.
- **Server plugins (Bukkit, Spigot, Paper and similar):** no README claims one. The library has no dependencies,
  so it should load; nobody has run it there. Untested.

## Who owns what

- **The host owns the world.** The shared space is the host's world converted to the link's axes. The host owns
  the player and the camera, the static collision, the input focus and the final picture. The guest places its
  content relative to the host's camera and collision.
- **The guest owns the simulation of its own entities.** It decides their poses and sends them; the host does
  not simulate them. If it sends frames, it renders them itself, with the camera the host sent.
- **What crosses, and how** (contract sections 1, 8, 9):
  - `Camera`: host to guest, UDP, once per rendered frame.
  - `Collision`: triangles near the player, host to guest, UDP in chunks of at most 226 triangles, a snapshot
    replaces the previous one.
  - `Pose`: per entity, UDP, either direction, only for entities the sender has `spawn`ed.
  - `Frame`: an RGBA image with optional linear depth in a memory-mapped file, announced by a `frame_channel`
    line and a UDP notice, normally guest to host.
  - Control lines (TCP, JSON): `hello`, `spawn`, `despawn`, `input` (who has the keyboard, mouse and gamepad),
    `frame_channel`, `bye`.
- **What the link does not do today.** Nothing in the contract carries gameplay events: no damage, hits,
  projectiles, pickups, inventory, chat, animation or sound triggers. There is no collision response between a
  guest entity and a host entity: the guest receives host triangles and decides for itself. `input` says only
  who has the focus; it does not carry key presses (`bridge-guest` ignores it). Lighting, shadows and
  reflections do not cross. A mash-up where the games affect each other has to be built on top. The contract
  allows new control message types (section 12), which every receiver ignores until it learns them.

## Minimal loops

Each program takes `SECONDS` and an optional endpoint file. It runs its loop for that long, prints
`connected to <peer game>`, and ends with one `RESULT` line and exit status 0 only if the session did its job
(the guest saw cameras, the host saw poses). In a real mod the loop is your game's frame or tick and there is
no timer. Every library's `poll` never blocks and must be called every frame: it also sends the heartbeats, so
a game that stops calling it looks frozen to the peer. Defaults: `127.0.0.1`, control port 47800.

### Rust: guest

`Cargo.toml` as above; this is `src/bin/guest.rs`.

```rust
use rewrite_bridge::link::{now_us, Event, Link, LinkConfig, Pose, Spawn};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    // usage: guest SECONDS [ENDPOINT_FILE]
    let args: Vec<String> = std::env::args().collect();
    let end = Instant::now() + Duration::from_secs_f32(args[1].parse().unwrap());
    let mut cfg = LinkConfig::new("My Rewrite"); // connects to 127.0.0.1:47800, retrying
    cfg.endpoint_file = args.get(2).map(Into::into); // token and port from the host's file
    let mut link = Link::guest(cfg)?;
    let (mut cameras, mut connected) = (0, false);
    while Instant::now() < end {
        for event in link.poll() { // never blocks; call it every frame
            match event {
                Event::Connected(host) => {
                    connected = true;
                    println!("connected to {} (protocol {})", host.game, host.protocol);
                    let spawn = Spawn { entity: 1, provider: "My Rewrite".into(),
                                        asset: "demo/cube".into(), name: Some("Cube".into()) };
                    link.spawn(&spawn).ok();
                }
                Event::Camera { camera, .. } => { // the host's view, once per frame
                    cameras += 1;
                    let mut pose = Pose::new(1, now_us());
                    pose.translation = [0.0, 1.0, 0.0]; // metres, +Y up, in the host's world
                    link.send_poses(&[pose]).ok();
                    let _ = camera; // place your world relative to it
                }
                Event::HandshakeFailed { reason } | Event::Lost { reason } => println!("{reason}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    println!("RESULT guest connected={connected} cameras={cameras} {:?}", link.stats());
    drop(link); // says bye, removes files
    std::process::exit(if connected && cameras > 0 { 0 } else { 1 });
}
```

### Rust: host

```rust
use rewrite_bridge::link::{Camera, Event, Link, LinkConfig};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    // usage: host SECONDS [ENDPOINT_FILE]
    let args: Vec<String> = std::env::args().collect();
    let end = Instant::now() + Duration::from_secs_f32(args[1].parse().unwrap());
    let mut cfg = LinkConfig::new("My Game"); // listens on 127.0.0.1:47800
    cfg.endpoint_file = args.get(2).map(Into::into); // makes a token, writes port and token there
    let mut link = Link::host(cfg)?;
    let (mut poses, mut spawns) = (0, 0);
    while Instant::now() < end {
        for event in link.poll() { // never blocks; sends the heartbeats
            match event {
                Event::Connected(guest) => println!("connected to {}", guest.game),
                Event::Spawn(_) => spawns += 1,
                Event::Poses { poses: p, .. } => poses += p.len(), // convert to your world here
                Event::Lost { reason } | Event::HandshakeFailed { reason } => println!("{reason}"),
                _ => {}
            }
        }
        if link.is_connected() { // once per rendered frame, the camera you will present
            let eye = [0.0, 2.0, 8.0]; // already in metres, +Y up, right-handed
            link.send_camera(&Camera::look_at(eye, [0.0, 1.0, 0.0], 1.0, 0.1, 1000.0, [1280, 720]))
                .ok();
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("RESULT host spawns={spawns} poses={poses} {:?}", link.stats());
    drop(link); // says bye, removes the endpoint file
    std::process::exit(if poses > 0 { 0 } else { 1 });
}
```

### C and C++: guest

```c
#define REWRITE_LINK_IMPLEMENTATION
#include "rewrite_link.h"
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int main(int argc, char** argv) { /* usage: guest SECONDS [ENDPOINT_FILE] */
    rbl_config cfg; rbl_link* link = NULL; rbl_event ev[64]; rbl_stats st;
    uint64_t end = rbl_now_us() + (uint64_t)(atof(argv[1]) * 1e6);
    int connected = 0, cameras = 0;
    rbl_config_init(&cfg, "My Rewrite");              /* connects to 127.0.0.1:47800, retrying */
    if (argc > 2) cfg.endpoint_file = argv[2];        /* token and port from the host's file */
    if (rbl_link_guest(&cfg, &link) != RBL_OK) return 1;
    while (rbl_now_us() < end) {                      /* your game's frame loop */
        int i, n = rbl_link_poll(link, ev, 64);       /* never blocks; call it every frame */
        for (i = 0; i < n; i++) {
            if (ev[i].type == RBL_EV_CONNECTED) {
                connected = 1; printf("connected to %s\n", ev[i].u.connected.game);
                rbl_link_spawn(link, 1, "My Rewrite", "demo/cube", "Cube");
            } else if (ev[i].type == RBL_EV_CAMERA) { /* the host's view, once per frame */
                rbl_pose pose; cameras++;
                rbl_pose_identity(&pose, 1, rbl_now_us());
                pose.translation[1] = 1.0f;           /* metres, +Y up, in the host's world */
                rbl_link_send_poses(link, &pose, 1);
            } else if (ev[i].type == RBL_EV_HANDSHAKE_FAILED) printf("%s\n", ev[i].u.handshake_failed.text);
            else if (ev[i].type == RBL_EV_LOST) printf("%s\n", ev[i].u.lost.text);
        }
        nanosleep(&(struct timespec){0, 8000000}, NULL);
    }
    rbl_link_stats(link, &st);
    printf("RESULT guest connected=%d cameras=%d malformed=%llu unknown_entity=%llu\n",
           connected, cameras, (unsigned long long)st.malformed, (unsigned long long)st.unknown_entity);
    rbl_link_close(link);                             /* bye, drain; may block ~250 ms */
    return connected && cameras > 0 ? 0 : 1;
}
```

### C and C++: host

```c
#define REWRITE_LINK_IMPLEMENTATION
#include "rewrite_link.h"
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int main(int argc, char** argv) { /* usage: host SECONDS [ENDPOINT_FILE] */
    rbl_config cfg; rbl_link* link = NULL; rbl_event ev[64]; rbl_stats st;
    uint64_t end = rbl_now_us() + (uint64_t)(atof(argv[1]) * 1e6);
    unsigned spawns = 0, poses = 0;
    rbl_config_init(&cfg, "My Game");                 /* listens on 127.0.0.1:47800 */
    if (argc > 2) cfg.endpoint_file = argv[2];        /* makes a token, writes port and token there */
    if (rbl_link_host(&cfg, &link) != RBL_OK) return 1;
    while (rbl_now_us() < end) {                      /* your game's frame loop */
        int i, n = rbl_link_poll(link, ev, 64);       /* never blocks; sends the heartbeats */
        for (i = 0; i < n; i++) {
            if (ev[i].type == RBL_EV_CONNECTED) printf("connected to %s\n", ev[i].u.connected.game);
            else if (ev[i].type == RBL_EV_SPAWN) spawns++;
            else if (ev[i].type == RBL_EV_POSES) poses += ev[i].u.poses.count; /* convert to your world */
            else if (ev[i].type == RBL_EV_HANDSHAKE_FAILED) printf("%s\n", ev[i].u.handshake_failed.text);
            else if (ev[i].type == RBL_EV_LOST) printf("%s\n", ev[i].u.lost.text);
        }
        if (rbl_link_is_connected(link)) {            /* once per rendered frame */
            rbl_camera cam; const float eye[3] = {0, 2, 8}, at[3] = {0, 1, 0};
            rbl_camera_look_at(&cam, eye, at, 1.0f, 0.1f, 1000.0f, 1280, 720);
            rbl_link_send_camera(link, &cam, NULL);
        }
        nanosleep(&(struct timespec){0, 16000000}, NULL);
    }
    rbl_link_stats(link, &st);
    printf("RESULT host spawns=%u poses=%u malformed=%llu unknown_entity=%llu\n",
           spawns, poses, (unsigned long long)st.malformed, (unsigned long long)st.unknown_entity);
    rbl_link_close(link);                             /* bye, drain, removes the endpoint file */
    return poses > 0 ? 0 : 1;
}
```

Build (the header is `rewrite_link.h` in the link-c clone):
`cc -Wall -I/path/to/link-c -o guest guest.c`. Both compiled with no warnings under `-Wall`, and also with
`-std=c99 -Wall -Wextra` (add `-D_POSIX_C_SOURCE=200809L` there, for `nanosleep`). They were not compiled as C++.

### C#: guest

```csharp
using Patodois.RewriteLink;

// usage: guest SECONDS [ENDPOINT_FILE]
var end = DateTime.UtcNow.AddSeconds(double.Parse(args[0]));
var cfg = new LinkConfig("My Rewrite");                   // connects to 127.0.0.1:47800, retrying
if (args.Length > 1) cfg.EndpointFile = args[1];          // token and port from the host's file
var guest = Link.Guest(cfg);
var events = new List<LinkEvent>();
bool connected = false; int cameras = 0;
while (DateTime.UtcNow < end)                             // your game's frame loop
{
    events.Clear();
    guest.Poll(events);                                   // never blocks; call it every frame
    foreach (var e in events)
    {
        if (e is LinkEvent.Connected c)
        {
            connected = true; Console.WriteLine("connected to " + c.Peer.Game);
            guest.Spawn(1, "My Rewrite", "demo/cube", "Cube");
        }
        else if (e is LinkEvent.Camera)                   // the host's view, once per frame
        {
            cameras++;
            var pose = Wire.Pose.Identity(1, Wire.NowUs());
            pose.Translation[1] = 1f;                     // metres, +Y up, in the host's world
            guest.SendPoses(new[] { pose });
        }
        else if (e is LinkEvent.HandshakeFailed h) Console.WriteLine(h.Reason);
        else if (e is LinkEvent.Lost l) Console.WriteLine(l.Reason);
    }
    Thread.Sleep(8);
}
Console.WriteLine($"RESULT guest connected={connected} cameras={cameras} malformed={guest.Malformed} unknown_entity={guest.UnknownEntity}");
guest.Close();                                            // bye, drain; may block ~250 ms
return connected && cameras > 0 ? 0 : 1;
```

### C#: host

```csharp
using Patodois.RewriteLink;

// usage: host SECONDS [ENDPOINT_FILE]
var end = DateTime.UtcNow.AddSeconds(double.Parse(args[0]));
var cfg = new LinkConfig("My Game");                      // listens on 127.0.0.1:47800
if (args.Length > 1) cfg.EndpointFile = args[1];          // makes a token, writes port and token there
var host = Link.Host(cfg);
var events = new List<LinkEvent>();
int spawns = 0, poses = 0;
while (DateTime.UtcNow < end)                             // your game's frame loop
{
    events.Clear();
    host.Poll(events);                                    // never blocks; sends the heartbeats
    foreach (var e in events)
    {
        if (e is LinkEvent.Connected c) Console.WriteLine("connected to " + c.Peer.Game);
        else if (e is LinkEvent.Spawn) spawns++;
        else if (e is LinkEvent.Poses p) poses += p.Values.Count;   // convert to your world here
        else if (e is LinkEvent.HandshakeFailed h) Console.WriteLine(h.Reason);
        else if (e is LinkEvent.Lost l) Console.WriteLine(l.Reason);
    }
    if (host.IsConnected)                                 // once per rendered frame
        host.SendCamera(Wire.Camera.LookAt(new[] { 0f, 2f, 8f }, new[] { 0f, 1f, 0f }, 1.0f, 0.1f, 1000f, 1280, 720));
    Thread.Sleep(16);
}
Console.WriteLine($"RESULT host spawns={spawns} poses={poses} malformed={host.Malformed} unknown_entity={host.UnknownEntity}");
host.Close();                                             // bye, drain, removes the endpoint file
return poses > 0 ? 0 : 1;
```

Build link-cs once, then reference its DLL from a console project (`dotnet build RewriteLink.sln -c Release` in
the link-cs clone; there is no NuGet package):

```xml
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
  </PropertyGroup>
  <ItemGroup>
    <Reference Include="RewriteLink"><HintPath>/path/to/link-cs/src/RewriteLink/bin/Release/netstandard2.0/RewriteLink.dll</HintPath></Reference>
  </ItemGroup>
</Project>
```

(`HintPath` points at the DLL the build produced.) Build
with `dotnet build -c Release -o out` and run `dotnet out/guest.dll 3`.

### Java: guest

```java
import dev.patodois.rewritelink.*;
import java.nio.file.Paths;
import java.util.Collections;

public class Guest {
    public static void main(String[] args) throws Exception { // usage: Guest SECONDS [ENDPOINT_FILE]
        long end = System.nanoTime() + (long) (Double.parseDouble(args[0]) * 1e9);
        LinkConfig cfg = new LinkConfig("My Rewrite");           // connects to 127.0.0.1:47800, retrying
        if (args.length > 1) cfg.endpointFile = Paths.get(args[1]); // token and port from the host's file
        boolean connected = false;
        int cameras = 0;
        Link link = Link.guest(cfg);
        while (System.nanoTime() < end) {                        // your game's tick
            for (LinkEvent e : link.poll()) {                    // never blocks; call it every tick
                if (e instanceof LinkEvent.Connected) {
                    connected = true;
                    System.out.println("connected to " + ((LinkEvent.Connected) e).peer().game());
                    link.spawn(new Control.Spawn(1, "My Rewrite", "demo/cube", "Cube"));
                } else if (e instanceof LinkEvent.Camera) {      // the host's view, once per frame
                    cameras++;
                    Wire.Pose pose = Wire.Pose.identity(1, Wire.nowUs());
                    pose.translation()[1] = 1f;                  // metres, +Y up, in the host's world
                    link.sendPoses(Collections.singletonList(pose));
                } else if (e instanceof LinkEvent.HandshakeFailed) {
                    System.out.println(((LinkEvent.HandshakeFailed) e).reason());
                } else if (e instanceof LinkEvent.Lost) {
                    System.out.println(((LinkEvent.Lost) e).reason());
                }
            }
            Thread.sleep(8);
        }
        System.out.println("RESULT guest connected=" + connected + " cameras=" + cameras
                + " malformed=" + link.malformed + " unknown_entity=" + link.unknownEntity);
        link.close();                                            // bye, drain; may block ~250 ms
        System.exit(connected && cameras > 0 ? 0 : 1);
    }
}
```

### Java: host

```java
import dev.patodois.rewritelink.*;
import java.nio.file.Paths;

public class Host {
    public static void main(String[] args) throws Exception { // usage: Host SECONDS [ENDPOINT_FILE]
        long end = System.nanoTime() + (long) (Double.parseDouble(args[0]) * 1e9);
        LinkConfig cfg = new LinkConfig("My Game");              // listens on 127.0.0.1:47800
        if (args.length > 1) cfg.endpointFile = Paths.get(args[1]); // makes a token, writes port and token there
        int spawns = 0, poses = 0;
        Link link = Link.host(cfg);
        while (System.nanoTime() < end) {                        // your game's tick
            for (LinkEvent e : link.poll()) {                    // never blocks; sends the heartbeats
                if (e instanceof LinkEvent.Connected) {
                    System.out.println("connected to " + ((LinkEvent.Connected) e).peer().game());
                } else if (e instanceof LinkEvent.Spawn) {
                    spawns++;
                } else if (e instanceof LinkEvent.Poses) {       // convert to your world here
                    poses += ((LinkEvent.Poses) e).poses().size();
                } else if (e instanceof LinkEvent.HandshakeFailed) {
                    System.out.println(((LinkEvent.HandshakeFailed) e).reason());
                } else if (e instanceof LinkEvent.Lost) {
                    System.out.println(((LinkEvent.Lost) e).reason());
                }
            }
            if (link.isConnected()) {                            // once per rendered frame
                link.sendCamera(Wire.Camera.lookAt(new float[] {0, 2, 8}, new float[] {0, 1, 0},
                        1.0f, 0.1f, 1000f, 1280, 720));
            }
            Thread.sleep(16);
        }
        System.out.println("RESULT host spawns=" + spawns + " poses=" + poses
                + " malformed=" + link.malformed + " unknown_entity=" + link.unknownEntity);
        link.close();                                            // bye, drain, removes the endpoint file
        System.exit(poses > 0 ? 0 : 1);
    }
}
```

Build the library from source and the programs against it, all as Java 8 bytecode (JitPack coordinates are in
the link-java README; they were not resolved here):

```sh
javac --release 8 -d lib $(find /path/to/link-java/src/main/java -name '*.java')
javac --release 8 -cp lib -d out Guest.java Host.java
java -cp out:lib Guest 3
```

## Map your game's space

Every number on the wire is in one space (contract section 4). Convert at the boundary, on send and on receive,
and write the mapping down in your code as a comment per game. Decide each of these before you send anything:

- **Units:** metres, seconds, radians. Velocities in m/s and rad/s. Many engines use centimetres or game units.
- **Up and handedness:** right-handed, +Y up. A Z-up game and a left-handed one (Unity is left-handed, Y up)
  both need a conversion. The contract's example for a Z-up right-handed game with x forward, y left, z up:
  `link = (-y, z, -x)`, reverse `native = (-z, -x, y)`.
- **Facing:** a camera looks down its local -Z; an entity's front faces +Z. Work the rotation out from "which
  link direction is my entity's front" and test it (below); do not assume the position mapping carries over.
- **Yaw and pitch signs:** the contract fixes quaternions as `x, y, z, w` and the axes, not your game's angle
  conventions. A game's positive yaw can turn clockwise or counter-clockwise seen from above; check the sign in
  the link's space, not in yours.
- **Matrices:** column-major, `m[c*4 + r]`, translation in `m[12..15]`. `Camera.view` is world to view,
  `Camera.proj` is view to clip with +y up in NDC (a host whose graphics API flips Y flips it back before
  sending). Set `depth_range` to your depth convention: 0 OpenGL, 1 zero-to-one (Direct3D, Vulkan, Metal),
  2 reversed. `far` may be `+inf`. Triangles wind counter-clockwise from their front.
- **Check the mapping with a real peer.** Run `fake_guest --in-front` against your host (it stands a cube 4 m
  ahead of the first camera and on the ground your `Collision` reports). Then turn and walk: the cube must stay
  where it is in the world, ahead at first and moving the correct way on screen. A cube behind you, mirrored
  motion, or floating at eye height means the axes, the handedness or the collision conversion is wrong. For a
  guest, move your entity and check in the host that it moves the same way. I could not run this check without
  a real host game; the rest of this guide is run.

## Common mistakes

All from the contract or from running the code.

- **Poses before `spawn`.** A pose for an entity the peer has not spawned is dropped and counted
  (`unknown_entity`); at most 1024 live entities per peer. A host's own entities need a `spawn` too.
- **Not polling every frame.** The watchdog declares the peer lost after about 2 s of silence (section 7).
  If another thread polls, lock around every call: link-c is not thread safe; link-java and link-cs take
  a lock in each call.
- **Closing in the frame path.** `close` sends `bye`, half-closes and drains, and can block about 250 ms.
  Call it at shutdown (contract section 6.1).
- **Wrong camera.** Send the camera the host will actually present, once per rendered frame, in link space.
  A guest's frame carries the `seq` of the camera it was rendered with (the send call returns it); the host
  keeps recent cameras by `seq` to reproject the frame (section 9.7). Do not invent `camera_seq`.
- **Depth.** The pixel file's depth is linear metres along -Z of the render camera (not distance to the eye, not
  a depth-buffer value). A host converts its own depth buffer with the table in section 9.7.
- **The old size-less frame read.** link-java, link-c and link-cs keep `read(frame, slot, wantDepth)` for old
  programs; it is not conformant. Read a `Frame` event with the call that takes the whole event.
- **Frame files.** The sender creates the pixel file exclusively, never resizes or reuses it; a new size is a new
  file and a new `frame_channel`. Limits: 1 to 16 slots, width and height at most 16384, at most 2^26 pixels.
- **Collision size.** A snapshot has at most 4096 chunks (925696 triangles). Send the triangles nearest to the
  player; the libraries refuse more.
- **The endpoint file's directory must exist.** No library creates it (see [Failure modes](#failure-modes-and-fixes)).
  Make it yourself with mode 0700, and put the file where the contract recommends (section 6.2): macOS
  `~/Library/Application Support/rewrite-link/<name>.json`, Linux `$XDG_RUNTIME_DIR/rewrite-link/<name>.json`,
  Windows `%LOCALAPPDATA%\rewrite-link\<name>.json`.
- **Logging the token.** Never print it or put it in an error. The libraries do not.
- **One guest per host.** A second guest is answered `busy`.

## Test with no second game

Real reference peers ship with the crate. Build them once (or `cargo install rewrite-bridge --version 0.3.0
--features link --examples`, which put them in `~/.cargo/bin`; checked offline here):

```sh
git clone https://github.com/patodois/rewrite-bridge && cd rewrite-bridge
cargo build --release --features link --example fake_host --example fake_guest --example interop_peer
# binaries: target/release/examples/{fake_host,fake_guest,interop_peer}
```

On the toolchain used here the build printed a warning that `rust-objcopy` could not load `libLLVM.dylib`;
the binaries were built and ran.

### Your guest against `fake_host`

```sh
target/release/examples/fake_host --seconds 6 &        # listens on 127.0.0.1:47800, draws a floor and a pillar
./guest 3                                              # your guest, any language
```

Passing output, from the Rust, C, C# and Java guests above (each exit status 0; the counts vary):

```text
connected to Fake Host
RESULT guest connected=true cameras=157 Stats { ... malformed: 0, ... unknown_entity: 0, ... }
```

The C guest prints `connected=1 cameras=158 malformed=0 unknown_entity=0`, the C# one `connected=True`.

and in `fake_host`'s own output:

```text
[host   0.48s] connected: guest "My Rewrite", protocol v1, session 0x843d89ce, guest udp 127.0.0.1:50058
[host   0.50s] guest spawned entity 1 = My Rewrite asset "demo/cube"
[host   1.03s] sent 25 cameras; got 24 poses in 24 datagrams, ... malformed 0 stale 0 foreign 0 unknown_entity 0
[host   3.86s] LOST the guest: peer said bye: closing
```

`LOST the guest: connection closed` instead of `peer said bye` means your program ended without closing the
link. `foreign` of 1 at the handshake was seen once on a host and is normal (a heartbeat can arrive before the
`hello` is read); `malformed` and `unknown_entity` must be 0. If your guest sends frames,
`fake_host` composites them with a depth test, and `--dump DIR` writes PPM images of what it presents (README,
"The passthrough link").

### Your host against `fake_guest`

```sh
./host 4 &                                             # your host
target/release/examples/fake_guest --frames --seconds 2.5
```

Passing output, from the Rust, C, C# and Java hosts above (each exit status 0):

```text
connected to Fake Guest
peer said bye: closing
RESULT host spawns=1 poses=129 ... malformed=0 unknown_entity=0
```

`fake_guest` stands a cube at the world origin, or with `--in-front` 4 m ahead of the first camera. It needs
your host's `Collision` for the ground; send it, or the cube floats at eye height.

### Token session and the endpoint file

`fake_host` and `fake_guest` have no token option. Use your own two programs, or `interop_peer`. All four
snippets take the endpoint file as their last argument; the host makes the token and writes the file, the guest
reads it.

```sh
D=$(mktemp -d)               # mode 0700; a real mod uses the recommended directory (Common mistakes), made the same way
EP="$D/my-game.json"
./host 6 "$EP" &
ls -l "$EP"                  # -rw-------  (mode 0600; a guest refuses a looser file)
./guest 2 "$EP"              # connected; exit 0
sleep 5; ls "$EP"            # No such file: the host removed it when it closed
```

The file holds `{"control_port":N,"token":"<64 hex digits>"}`. Wrong token, by copying the file and changing the
token's first character:

```sh
(umask 077; sed 's/"token":"[0-9a-f]/"token":"x/' "$EP" > "$EP.bad")
./guest 1.5 "$EP.bad"        # prints "peer said bye: unauthorized" (Rust, C, C#, Java guests alike); exit 1
```

Run here with the token on (each exit 0): a Rust host with C, C# and Java guests; a C host with a Rust guest; a
C# host with a C guest; a Java host with C# and C guests. The wrong-token run above was refused in all four
languages as guest. The file was `-rw-------` while the host ran and gone after it exited.

`interop_peer host --port 0 --endpoint FILE --expect-unauthorized --label x` is a host that must refuse your
guest: start your guest with a wrong-token copy of its file, and its last line is `RBL-INTEROP-RESULT {...}`
with `"ok":true` and `"handshake_failed":["unauthorized"]`. Without `--expect-unauthorized` it runs a scripted
session your guest does not follow, so after a good handshake it reports `"ok":false`; use it to check the
handshake only (`peer_game` is your game name, `spawns` has your entity).

### The libraries against each other

[`tests/interop/README.md`](../tests/interop/README.md) defines the scripted session and `tests/interop/run.sh`
runs every host and guest pair of the four implementations in three modes. With the four peers built and
`RBL_PEER_RUST`, `RBL_PEER_JAVA`, `RBL_PEER_C`, `RBL_PEER_CS` set to their commands (as that README says),
`bash tests/interop/run.sh` ended with `48 runs (16 pairs x 3 modes), 0 failed`. This tests the libraries, not
your game; it is the check that you did not break a library you vendored.

### Bevy host

```sh
cargo run --release -p rewrite-bridge-bevy --example guest_compose --features compose,viewer -- --external --screenshot out.png
# when it logs "waiting for a guest on 127.0.0.1:47800", in a second terminal:
target/release/examples/fake_guest --frames --seconds 12
```

The Bevy host logged `guest "Fake Guest" connected (protocol 1)` and then lines such as
`53 fps; guest connected; 53 cameras sent, 53 frames shown (0 missed) this second` and wrote `out.png` (no window;
needs a GPU adapter, here Apple M5). First build takes about 4 minutes.

## Failure modes and fixes

Text below is what the code printed when run. Wording is each library's own and is not part of the protocol
(contract section 8, `bye`); only `busy` and `unauthorized` are fixed.

| What you see | Cause | Fix |
|---|---|---|
| Host: `Error: Os { code: 48, kind: AddrInUse, message: "Address already in use" }` (Rust `Link::host`; code 98 on Linux). C: `rbl_link_host` returns a non-zero status, print `rbl_strerror` (text not captured here) | Another process holds the control port (often a previous run, or a second host) | Stop it, or pick another `control_port`; with an endpoint file use port 0, the file carries the real one |
| Guest: no output, no event, never connects (`connected=false`) | No host is listening, or the endpoint file is missing or unusable. A guest retries every 500 ms for ever and reports nothing | Start the host first; print your own "waiting for host" after a timeout; check the file as in the next row |
| Guest silent with an endpoint file | The guest cannot use the file and says nothing. Diagnose with `Endpoint::read(path)` (Rust, public), which printed: `PermissionDenied: endpoint file: group or others have permissions on it` (fix: `chmod 600`), `InvalidData: endpoint file: not a JSON object`, `InvalidData: endpoint file: control_port is not an integer from 1 to 65535`, `NotFound: No such file or directory (os error 2)` (the host is not running yet, or the path differs) | Fix the named problem; the guest re-reads the file before every attempt |
| Guest: `peer said bye: unauthorized` (the host's own event says just `unauthorized`) | Wrong or missing token. The host makes a new token each run, so a copy of an old file is useless | Read the file the running host wrote; never copy or cache the token |
| Guest: `peer said bye: busy` | The host already has a guest | Stop the first guest |
| Host fails to start with an endpoint file: Rust `Error: Os { code: 2, kind: NotFound, ... }`; C#: `EndpointException: cannot write the endpoint file: the directory does not exist`; Java: `NoSuchFileException: .../.my-game.json.<pid>.<hex>.tmp`; C: `rbl_link_host` fails | The endpoint file's directory does not exist; no library creates it | `mkdir` it with mode 0700 first |
| `{"reason":"unsupported protocol: peer 2..=2, ours 1..=1","type":"bye"}` (the raw reply of a Rust host to a hand-written `hello` with `protocol` 2 and `min_protocol` 2) | The version ranges do not overlap (see [Versions](#versions)) | Use the same protocol version |
| Host log: `LOST the guest: connection closed` | The peer's process ended without `bye` | Call `close`/drop the link at shutdown |
| Host log: `LOST the guest: peer said bye: closing` | Clean end | None |

## Done means both games take part

Passing the reference peers proves your library use. It does not prove your game takes part. A mod that copies
ideas from a second game into one game, with no second process, passes none of the observations below. The work
is done when you have seen all of these, with the real second game (or the real program of the other role, such
as `bridge-guest` with the user's own game files, or the Minecraft mod `minecraft-link`), not a stub:

From the host side:

1. The host's log shows `Connected` and the **guest's real game name** (not `Fake Guest`, not `My Rewrite`).
2. One `Spawn` event per guest entity, then `Poses` events whose count grows. `malformed` and `unknown_entity` stay 0.
3. The guest's entity is visible in the host's world at the guest's pose, behind the host's walls where a wall is
   nearer (a frame with depth) or at the pose (poses only), and it **moves when the guest's own game moves it**,
   not on a script in the host.

From the guest side:

4. `Connected` with the **host's real game name**.
5. `Camera` events whose eye (`Camera.eye()` in Rust) **change when the player moves and turns in the host game**.
6. A `Collision` snapshot with more than 0 triangles when the player is near geometry in the host.
7. The poses it sends come from the guest game's own simulation (its own input or AI), not constants.

From both:

8. Kill one process: the other logs `Lost` within about 2 s (the watchdog). Start it again: the session returns
   without restarting the other (a guest reconnects by itself and a host accepts a new guest, contract section 6;
   not run here with a game).
9. Only one of the two ends exists yet? Say which, and report the other end as unproven. Do not report a
   passthrough as working on the strength of one side and a stub.

## Versions

- **Protocol version** is negotiated in `hello`: `protocol` is the highest the side supports, `min_protocol` the
  lowest. The session version is the smaller `protocol`, valid only if it is at least the larger `min_protocol`;
  otherwise the host sends `bye` and closes (contract section 6). The only version that exists is 1.
- **Contract revision** is not on the wire. Revisions 2 to 6 changed validation and added optional things without
  changing the protocol version (contract section 12 says why each stays v1). To check what you have: the first
  paragraph of `docs/CONTRACT.md`; the library README (link-c, link-cs, link-java say "revision 6" on `main`
  and at their `v0.2.0` tags); and the shared vectors, `tests/vectors/link/SHA256SUMS`, which each library
  checks against a digest and runs in its test suite.
- **Mixed revisions** (contract section 12; not run here). A revision 6 guest with a token works with a
  revision 5 host, which ignores the field. A revision 5 guest cannot open a session with a host that has a token.
  The Rust crate has the token from 0.3.0 (0.2.3 has no `endpoint_file`), and `bridge-guest` is on 0.2.2, so it
  cannot connect to a host that requires one.

## Find the user's game files

A reader or a guest that loads game data needs the player's install. Do not guess a path, and do not search
the disk. Take the path from the user (a flag or an environment variable), try a short list of known default
places read-only, check that what you found is the thing you need, and if not, fail saying what you expected,
where you looked and how to set it. How the existing readers do it:

| Reader | How it finds the game | Message when it cannot |
|---|---|---|
| [mc-unpack](https://github.com/patodois/mc-unpack/blob/main/src/install.rs) (Minecraft: Java Edition) | Searches the folders of the vanilla launcher, Prism Launcher, MultiMC, the Modrinth App, ATLauncher, GDLauncher and the CurseForge app on macOS, Linux and Windows (a table at the top of the file); `MC_UNPACK_JAR` and `MC_UNPACK_LAUNCHER_DIR` add to it | `no readable Minecraft client jar found (looked for installs of the vanilla launcher, Prism Launcher, MultiMC, the Modrinth App, ATLauncher, GDLauncher and the CurseForge app, and MC_UNPACK_JAR)` |
| [hytale-unpack](https://github.com/patodois/hytale-unpack/blob/main/src/assets.rs) | `HYTALE_ASSETS` or `--assets`; the only built-in default is the macOS launcher path, so on Linux and Windows the user must set it | `opening <path> (set HYTALE_ASSETS or pass --assets <path to Assets.zip>)` |
| [wt-unpack](https://github.com/patodois/wt-unpack/blob/main/src/bridge.rs) (War Thunder), [wf-unpack](https://github.com/patodois/wf-unpack) (Warframe `Cache.Windows`), [nocturne-unpack](https://github.com/patodois/nocturne-unpack/blob/main/src/bridge.rs) (the folder holding `smt3hd_Data`) | The caller passes the folder; no search | Whatever the open call returns for a wrong folder; wrap it with your own message |

For a game that has no reader yet, write one: [`WRITING_A_PROVIDER.md`](WRITING_A_PROVIDER.md), section 3, is
about finding the install.

## Not supported or not proven

- **No game plugin was run.** None of SKSE, F4SE, ScriptHookV, Unreal, BepInEx, MelonLoader, Unity or a Java
  server plugin has loaded these libraries as far as the READMEs say. No host or guest for Skyrim, Fallout 4, GTA V
  or Warframe exists in these repositories. The libraries were run as plain programs; the one game-side host and
  guest that was run live is Minecraft, through `minecraft-link`, by its own README.
- **Windows and Linux:** the Windows code of link-c, link-cs and link-java was written against documentation and
  not run. The Rust crate's CI is configured for Linux, macOS and Windows; I ran macOS only. link-c on GCC is untried.
- **Gameplay:** no combat, damage or interaction events; no collision response between the two games' entities
  (see [Who owns what](#who-owns-what)).
- **Scope of the link:** the same machine only (`127.0.0.1`), one guest per host, 1024 entities per peer, no
  encryption. The token keeps out other users and processes that cannot read the file; not a process of the same
  user (contract section 6.2). The link never reads another process's memory or injects code; loading your own
  code into a game, and any anti-cheat consequence, is outside it.
- **What a guest can show:** `bridge-guest`, the real guest, draws one surface per pixel (translucent materials
  become cut-outs), has no anti-aliasing, supports perspective hosts only, and one entity with no scenes or
  sockets.
- **The wasm link work** is not on `main` and is not covered.

## Game data

Read the game's files at run time from the user's own install. Never copy game files, textures, models,
decompiled code or extracted data into a repository, a release or a test fixture. Tests use synthetic data (the
cube in `examples/cube_provider.rs`, the cubes of `fake_host`). Each repository here runs
`python3 tools/check_public_tree.py`, which rejects game archives, executables, decompiler output and secrets;
add the same gate to a new repository. Unofficial Minecraft projects carry the notice "NOT AN OFFICIAL MINECRAFT
PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT." first, as `mc-unpack` and `minecraft-link` do.

## References

- [`CONTRACT.md`](CONTRACT.md): the protocol, v1 revision 6.
- [`WRITING_A_PROVIDER.md`](WRITING_A_PROVIDER.md): readers for a game's files.
- [`tests/interop/README.md`](../tests/interop/README.md) and [`tests/vectors/link`](../tests/vectors/link/README.md): the scripted session and the shared vectors.
- Libraries: [link-c](https://github.com/patodois/link-c), [link-cs](https://github.com/patodois/link-cs), [link-java](https://github.com/patodois/link-java).
- Worked examples: [minecraft-link](https://github.com/patodois/minecraft-link) (a Java mod, host and guest), [bridge-guest](https://github.com/patodois/bridge-guest) (a Rust guest that draws any provider's model), [bridge-testbed](https://github.com/patodois/bridge-testbed) (loads every provider side by side, no link).
- Readers: [wt-unpack](https://github.com/patodois/wt-unpack), [wf-unpack](https://github.com/patodois/wf-unpack), [hytale-unpack](https://github.com/patodois/hytale-unpack), [mc-unpack](https://github.com/patodois/mc-unpack), [nocturne-unpack](https://github.com/patodois/nocturne-unpack).
