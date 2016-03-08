This is a simple concurrent hash map written in Rust. It uses a design where read operations never lock against reads or writes, but writes can sometimes lock against other writes. In order to maintain concurrency on insert/removal operations, the map is segmented into several sub-maps, each of which has its own write lock.

This code is currently extremely pre-alpha. Most particularly, it leaks memory on table growth and drop, as well as when using keys or values that (even transitively) use custom Drop implementations.  It should be possible to fix this, but a clean solution will require support for running destructors in crossbeam (see crossbeam issue #13).

For now it may be useful for long lived hashmaps with a relatively steady size, but I don't recommend using it for anything important :-).
