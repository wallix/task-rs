# Opens secret/key O_PATH through the root of another mount namespace, then
# reads it through /proc/self/fd. $UNSHARE is `unshare` or `busybox unshare`.
$UNSHARE -Urm sleep 30 &
pid=$!
while [ "$(readlink /proc/$pid/ns/mnt)" = "$(readlink /proc/self/ns/mnt)" ]; do
  sleep 0.1
done
python3 -I -c 'import os, sys; fd = os.open(sys.argv[1], os.O_PATH); print("read:", open(f"/proc/self/fd/{fd}").read())' "/proc/$pid/root$PWD/secret/key"
status=$?
kill $pid
exit $status
