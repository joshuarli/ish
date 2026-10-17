# Large heredoc with expansion — tests heredoc body handling
i=0
while [ $i -lt 200 ]; do
    cat <<EOF > /dev/null
line $i: The quick brown fox jumps over the lazy dog.
variable expansion: $i and $(echo nested_$i)
more text to fill the buffer and test throughput performance
EOF
    i=$((i + 1))
done
echo "heredoc done: $i"
