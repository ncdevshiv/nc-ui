- **The asset server's accept loop no longer trades idle CPU against accept latency.** It
blocked in a 250 ms channel receive between connections, and an *incoming* connection cannot
wake that receive — only a stop could — so every fresh connection waited for the poll to
expire before `accept()` ran. Measured over a real socket, a page's asset fetch cost a p50 of
~253,000 µs; the same fetch now costs ~560 µs, because the accept blocks in the kernel and a
stop wakes it by opening one throwaway loopback connection (`AssetServer::stop`) rather than
by a timed poll. Both goals the poll traded against are now met at once: idle CPU stays at
0.00 % (the accept parks in the kernel, the same as the runtimes the comparison measures
against) *and* a connection is answered the moment it arrives. Verified end to end by the
34 real-window tests in `ncui-runtime`'s `application` suite, which serve assets over a real
socket and drive the page bridge.
