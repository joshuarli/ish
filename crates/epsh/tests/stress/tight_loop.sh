# Tight loop with arithmetic — tests eval dispatch and variable assignment
i=0
while [ $i -lt 10000 ]; do
    i=$((i + 1))
done
echo "loop done: $i"
