OxideBSD

I honestly don't expect this to go far.
It's just an experiment to see if I can make a BSD from scratch in Rust, and by me I mean Claude.
If this goes well I will be happy.
Currently it can run a lot of the Busybox applets, and is in Phase 1 of ROADMAP.md's roadmap.

The kernel is a highly modular monolithic kernel, to allow isolating attack surface even if it is wide, such that exploits can be isolated and fixed quickly.
This technique also speeds up development as it allows critical system components to be developed seperately from the kernel.
It also removes one of the key disadvantages of modularity, as it allows components to run at the same level as the kernel.
