# Pipeline stages — tests fork/pipe/wait
i=0
while [ $i -lt 200 ]; do
    echo "line $i" | cat | cat | cat > /dev/null
    i=$((i + 1))
done
echo "pipeline done: $i"
