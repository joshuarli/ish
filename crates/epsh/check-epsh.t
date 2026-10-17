
name: selftest-1
description:
	Regression test self-testing
stdin:
	echo ${foo:-baz}
expected-stdout:
	baz
---

name: selftest-2
description:
	Regression test self-testing
env-setup: !foo=bar!
stdin:
	echo ${foo:-baz}
expected-stdout:
	bar
---

name: selftest-3
description:
	Regression test self-testing
env-setup: !ENV=fnord!
stdin:
	echo "<$ENV>"
expected-stdout:
	<fnord>
---

name: selftest-tools
description:
	Check that relevant tools work as expected. If not, e.g. on SerenityOS,
	install better tools from ports and prepend /usr/local/bin to $PATH.
stdin:
	echo foobarbaz | grep bar
	echo = $?
	echo abc | sed y/ac/AC/
	echo = $?
	echo abc | tr ac AC
	echo = $?
expected-stdout:
	foobarbaz
	= 0
	AbC
	= 0
	AbC
	= 0
---

name: arith-lazy-3
description:
	Check that assignments not done on non-evaluated side of ternary
	operator and this construct is parsed correctly (Debian #445651)
stdin:
	x=4
	y=$((0 ? x=1 : 2))
	echo = $x $y =
expected-stdout:
	= 4 2 =
---

name: arith-div-assoc-1
description:
	Check associativity of division operator
stdin:
	echo $((20 / 2 / 2))
expected-stdout:
	5
---

name: arith-div-intmin-by-minusone
description:
	Check division overflow wraps around silently
category: shell:legacy-no
stdin:
	echo signed:$((-2147483648 / -1))r$((-2147483648 % -1)).
	echo unsigned:$((# -2147483648 / -1))r$((# -2147483648 % -1)).
expected-stdout:
	signed:-2147483648r0.
	unsigned:0r2147483648.
---

name: bksl-nl-ign-1
description:
	Check that \newline is not collapsed after #
stdin:
	echo hi #there \
	echo folks
expected-stdout:
	hi
	folks
---

name: bksl-nl-ign-2
description:
	Check that \newline is not collapsed inside single quotes
stdin:
	echo 'hi \
	there'
	echo folks
expected-stdout:
	hi \
	there
	folks
---

name: bksl-nl-ign-3
description:
	Check that \newline is not collapsed inside single quotes
stdin:
	cat << \EOF
	hi \
	there
	EOF
expected-stdout:
	hi \
	there
---

#
# Places \newline should be collapsed
#
name: bksl-nl-1
description:
	Check that \newline is collapsed before, in the middle of, and
	after words
stdin:
	 	 	\
			 echo hi\
	There, \
	folks
expected-stdout:
	hiThere, folks
---

name: bksl-nl-2
description:
	Check that \newline is collapsed in $ sequences
	(ksh93 fails this)
stdin:
	a=12
	ab=19
	echo $\
	a
	echo $a\
	b
	echo $\
	{a}
	echo ${a\
	b}
	echo ${ab\
	}
expected-stdout:
	12
	19
	12
	19
	19
---

name: bksl-nl-3
description:
	Check that \newline is collapsed in $(..) and `...` sequences
	(ksh93 fails this)
stdin:
	echo $\
	(echo foobar1)
	echo $(\
	echo foobar2)
	echo $(echo foo\
	bar3)
	echo $(echo foobar4\
	)
	echo `
	echo stuff1`
	echo `echo st\
	uff2`
expected-stdout:
	foobar1
	foobar2
	foobar3
	foobar4
	stuff1
	stuff2
---

name: bksl-nl-4
description:
	Check that \newline is collapsed in $((..)) sequences
	(ksh93 fails this)
stdin:
	echo $\
	((1+2))
	echo $(\
	(1+2+3))
	echo $((\
	1+2+3+4))
	echo $((1+\
	2+3+4+5))
	echo $((1+2+3+4+5+6)\
	)
expected-stdout:
	3
	6
	10
	15
	21
---

name: bksl-nl-5
description:
	Check that \newline is collapsed in double quoted strings
stdin:
	echo "\
	hi"
	echo "foo\
	bar"
	echo "folks\
	"
expected-stdout:
	hi
	foobar
	folks
---

name: bksl-nl-7
description:
	Check that \newline is collapsed in double-quoted here-document
	delimiter.
stdin:
	a=12
	cat << "EO\
	F"
	a=$a
	foo\
	bar
	EOF
	echo done
expected-stdout:
	a=$a
	foo\
	bar
	done
---

name: bksl-nl-8
description:
	Check that \newline is collapsed in various 2+ character tokens
	delimiter.
	(ksh93 fails this)
stdin:
	echo hi &\
	& echo there
	echo foo |\
	| echo bar
	cat <\
	< EOF
	stuff
	EOF
	cat <\
	<\
	- EOF
		more stuff
	EOF
	cat <<\
	EOF
	abcdef
	EOF
	echo hi >\
	> /dev/null
	echo $?
	i=1
	case $i in
	(\
	x|\
	1\
	) echo hi;\
	;
	(*) echo oops
	esac
expected-stdout:
	hi
	there
	foo
	stuff
	more stuff
	abcdef
	0
	hi
---

name: bksl-nl-10
description:
	Check that \newline in a keyword is collapsed
stdin:
	i\
	f true; then\
	 echo pass; el\
	se echo fail; fi
expected-stdout:
	pass
---

name: break-1
description:
	See if break breaks out of loops
stdin:
	for i in a b c; do echo $i; break; echo bad-$i; done
	echo end-1
	for i in a b c; do echo $i; break 1; echo bad-$i; done
	echo end-2
	for i in a b c; do
	    for j in x y z; do
		echo $i:$j
		break
		echo bad-$i
	    done
	    echo end-$i
	done
	echo end-3
	for i in a b c; do echo $i; eval break; echo bad-$i; done
	echo end-4
expected-stdout:
	a
	end-1
	a
	end-2
	a:x
	end-a
	b:x
	end-b
	c:x
	end-c
	end-3
	a
	end-4
---

name: break-2
description:
	See if break breaks out of nested loops
stdin:
	for i in a b c; do
	    for j in x y z; do
		echo $i:$j
		break 2
		echo bad-$i
	    done
	    echo end-$i
	done
	echo end
expected-stdout:
	a:x
	end
---

name: break-5
description:
	Error if break argument isn't a number
stdin:
	for i in a b c; do echo $i; break abc; echo more-$i; done
	echo end
expected-stdout:
	a
expected-exit: e != 0
expected-stderr-pattern:
	/.*break.*/
---

name: continue-1
description:
	See if continue continues loops
stdin:
	for i in a b c; do echo $i; continue; echo bad-$i ; done
	echo end-1
	for i in a b c; do echo $i; continue 1; echo bad-$i; done
	echo end-2
	for i in a b c; do
	    for j in x y z; do
		echo $i:$j
		continue
		echo bad-$i-$j
	    done
	    echo end-$i
	done
	echo end-3
	for i in a b c; do echo $i; eval continue; echo bad-$i ; done
	echo end-4
expected-stdout:
	a
	b
	c
	end-1
	a
	b
	c
	end-2
	a:x
	a:y
	a:z
	end-a
	b:x
	b:y
	b:z
	end-b
	c:x
	c:y
	c:z
	end-c
	end-3
	a
	b
	c
	end-4
---

name: continue-2
description:
	See if continue breaks out of nested loops
stdin:
	for i in a b c; do
	    for j in x y z; do
		echo $i:$j
		continue 2
		echo bad-$i-$j
	    done
	    echo end-$i
	done
	echo end
expected-stdout:
	a:x
	b:x
	c:x
	end
---

name: continue-5
description:
	Error if continue argument isn't a number
stdin:
	for i in a b c; do echo $i; continue abc; echo more-$i; done
	echo end
expected-stdout:
	a
expected-exit: e != 0
expected-stderr-pattern:
	/.*continue.*/
---

name: expand-unglob-dblq
description:
	Check that regular "${foo+bar}" constructs are parsed correctly
stdin:
	u=x
	tl_norm() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo "$1 plus norm foo ${v+'bar'} baz")
		(echo "$1 dash norm foo ${v-'bar'} baz")
		(echo "$1 eqal norm foo ${v='bar'} baz")
		(echo "$1 qstn norm foo ${v?'bar'} baz") 2>/dev/null || \
		    echo "$1 qstn norm -> error"
		(echo "$1 PLUS norm foo ${v:+'bar'} baz")
		(echo "$1 DASH norm foo ${v:-'bar'} baz")
		(echo "$1 EQAL norm foo ${v:='bar'} baz")
		(echo "$1 QSTN norm foo ${v:?'bar'} baz") 2>/dev/null || \
		    echo "$1 QSTN norm -> error"
	}
	tl_paren() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo "$1 plus parn foo ${v+(bar)} baz")
		(echo "$1 dash parn foo ${v-(bar)} baz")
		(echo "$1 eqal parn foo ${v=(bar)} baz")
		(echo "$1 qstn parn foo ${v?(bar)} baz") 2>/dev/null || \
		    echo "$1 qstn parn -> error"
		(echo "$1 PLUS parn foo ${v:+(bar)} baz")
		(echo "$1 DASH parn foo ${v:-(bar)} baz")
		(echo "$1 EQAL parn foo ${v:=(bar)} baz")
		(echo "$1 QSTN parn foo ${v:?(bar)} baz") 2>/dev/null || \
		    echo "$1 QSTN parn -> error"
	}
	tl_brace() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo "$1 plus brac foo ${v+a$u{{{\}b} c ${v+d{}} baz")
		(echo "$1 dash brac foo ${v-a$u{{{\}b} c ${v-d{}} baz")
		(echo "$1 eqal brac foo ${v=a$u{{{\}b} c ${v=d{}} baz")
		(echo "$1 qstn brac foo ${v?a$u{{{\}b} c ${v?d{}} baz") 2>/dev/null || \
		    echo "$1 qstn brac -> error"
		(echo "$1 PLUS brac foo ${v:+a$u{{{\}b} c ${v:+d{}} baz")
		(echo "$1 DASH brac foo ${v:-a$u{{{\}b} c ${v:-d{}} baz")
		(echo "$1 EQAL brac foo ${v:=a$u{{{\}b} c ${v:=d{}} baz")
		(echo "$1 QSTN brac foo ${v:?a$u{{{\}b} c ${v:?d{}} baz") 2>/dev/null || \
		    echo "$1 QSTN brac -> error"
	}
	: '}}}' '}}}' '}}}' '}}}' '}}}' '}}}' '}}}' '}}}'
	tl_norm 1 -
	tl_norm 2 ''
	tl_norm 3 x
	tl_paren 4 -
	tl_paren 5 ''
	tl_paren 6 x
	tl_brace 7 -
	tl_brace 8 ''
	tl_brace 9 x
expected-stdout:
	1 plus norm foo  baz
	1 dash norm foo 'bar' baz
	1 eqal norm foo 'bar' baz
	1 qstn norm -> error
	1 PLUS norm foo  baz
	1 DASH norm foo 'bar' baz
	1 EQAL norm foo 'bar' baz
	1 QSTN norm -> error
	2 plus norm foo 'bar' baz
	2 dash norm foo  baz
	2 eqal norm foo  baz
	2 qstn norm foo  baz
	2 PLUS norm foo  baz
	2 DASH norm foo 'bar' baz
	2 EQAL norm foo 'bar' baz
	2 QSTN norm -> error
	3 plus norm foo 'bar' baz
	3 dash norm foo x baz
	3 eqal norm foo x baz
	3 qstn norm foo x baz
	3 PLUS norm foo 'bar' baz
	3 DASH norm foo x baz
	3 EQAL norm foo x baz
	3 QSTN norm foo x baz
	4 plus parn foo  baz
	4 dash parn foo (bar) baz
	4 eqal parn foo (bar) baz
	4 qstn parn -> error
	4 PLUS parn foo  baz
	4 DASH parn foo (bar) baz
	4 EQAL parn foo (bar) baz
	4 QSTN parn -> error
	5 plus parn foo (bar) baz
	5 dash parn foo  baz
	5 eqal parn foo  baz
	5 qstn parn foo  baz
	5 PLUS parn foo  baz
	5 DASH parn foo (bar) baz
	5 EQAL parn foo (bar) baz
	5 QSTN parn -> error
	6 plus parn foo (bar) baz
	6 dash parn foo x baz
	6 eqal parn foo x baz
	6 qstn parn foo x baz
	6 PLUS parn foo (bar) baz
	6 DASH parn foo x baz
	6 EQAL parn foo x baz
	6 QSTN parn foo x baz
	7 plus brac foo  c } baz
	7 dash brac foo ax{{{}b c d{} baz
	7 eqal brac foo ax{{{}b c ax{{{}b} baz
	7 qstn brac -> error
	7 PLUS brac foo  c } baz
	7 DASH brac foo ax{{{}b c d{} baz
	7 EQAL brac foo ax{{{}b c ax{{{}b} baz
	7 QSTN brac -> error
	8 plus brac foo ax{{{}b c d{} baz
	8 dash brac foo  c } baz
	8 eqal brac foo  c } baz
	8 qstn brac foo  c } baz
	8 PLUS brac foo  c } baz
	8 DASH brac foo ax{{{}b c d{} baz
	8 EQAL brac foo ax{{{}b c ax{{{}b} baz
	8 QSTN brac -> error
	9 plus brac foo ax{{{}b c d{} baz
	9 dash brac foo x c x} baz
	9 eqal brac foo x c x} baz
	9 qstn brac foo x c x} baz
	9 PLUS brac foo ax{{{}b c d{} baz
	9 DASH brac foo x c x} baz
	9 EQAL brac foo x c x} baz
	9 QSTN brac foo x c x} baz
---

name: expand-unglob-unq
description:
	Check that regular ${foo+bar} constructs are parsed correctly
stdin:
	u=x
	tl_norm() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo $1 plus norm foo ${v+'bar'} baz)
		(echo $1 dash norm foo ${v-'bar'} baz)
		(echo $1 eqal norm foo ${v='bar'} baz)
		(echo $1 qstn norm foo ${v?'bar'} baz) 2>/dev/null || \
		    echo "$1 qstn norm -> error"
		(echo $1 PLUS norm foo ${v:+'bar'} baz)
		(echo $1 DASH norm foo ${v:-'bar'} baz)
		(echo $1 EQAL norm foo ${v:='bar'} baz)
		(echo $1 QSTN norm foo ${v:?'bar'} baz) 2>/dev/null || \
		    echo "$1 QSTN norm -> error"
	}
	tl_paren() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo $1 plus parn foo ${v+\(bar')'} baz)
		(echo $1 dash parn foo ${v-\(bar')'} baz)
		(echo $1 eqal parn foo ${v=\(bar')'} baz)
		(echo $1 qstn parn foo ${v?\(bar')'} baz) 2>/dev/null || \
		    echo "$1 qstn parn -> error"
		(echo $1 PLUS parn foo ${v:+\(bar')'} baz)
		(echo $1 DASH parn foo ${v:-\(bar')'} baz)
		(echo $1 EQAL parn foo ${v:=\(bar')'} baz)
		(echo $1 QSTN parn foo ${v:?\(bar')'} baz) 2>/dev/null || \
		    echo "$1 QSTN parn -> error"
	}
	tl_brace() {
		v=$2
		test x"$v" = x"-" && unset v
		(echo $1 plus brac foo ${v+a$u{{{\}b} c ${v+d{}} baz)
		(echo $1 dash brac foo ${v-a$u{{{\}b} c ${v-d{}} baz)
		(echo $1 eqal brac foo ${v=a$u{{{\}b} c ${v=d{}} baz)
		(echo $1 qstn brac foo ${v?a$u{{{\}b} c ${v?d{}} baz) 2>/dev/null || \
		    echo "$1 qstn brac -> error"
		(echo $1 PLUS brac foo ${v:+a$u{{{\}b} c ${v:+d{}} baz)
		(echo $1 DASH brac foo ${v:-a$u{{{\}b} c ${v:-d{}} baz)
		(echo $1 EQAL brac foo ${v:=a$u{{{\}b} c ${v:=d{}} baz)
		(echo $1 QSTN brac foo ${v:?a$u{{{\}b} c ${v:?d{}} baz) 2>/dev/null || \
		    echo "$1 QSTN brac -> error"
	}
	: '}}}' '}}}' '}}}' '}}}' '}}}' '}}}' '}}}' '}}}'
	tl_norm 1 -
	tl_norm 2 ''
	tl_norm 3 x
	tl_paren 4 -
	tl_paren 5 ''
	tl_paren 6 x
	tl_brace 7 -
	tl_brace 8 ''
	tl_brace 9 x
expected-stdout:
	1 plus norm foo baz
	1 dash norm foo bar baz
	1 eqal norm foo bar baz
	1 qstn norm -> error
	1 PLUS norm foo baz
	1 DASH norm foo bar baz
	1 EQAL norm foo bar baz
	1 QSTN norm -> error
	2 plus norm foo bar baz
	2 dash norm foo baz
	2 eqal norm foo baz
	2 qstn norm foo baz
	2 PLUS norm foo baz
	2 DASH norm foo bar baz
	2 EQAL norm foo bar baz
	2 QSTN norm -> error
	3 plus norm foo bar baz
	3 dash norm foo x baz
	3 eqal norm foo x baz
	3 qstn norm foo x baz
	3 PLUS norm foo bar baz
	3 DASH norm foo x baz
	3 EQAL norm foo x baz
	3 QSTN norm foo x baz
	4 plus parn foo baz
	4 dash parn foo (bar) baz
	4 eqal parn foo (bar) baz
	4 qstn parn -> error
	4 PLUS parn foo baz
	4 DASH parn foo (bar) baz
	4 EQAL parn foo (bar) baz
	4 QSTN parn -> error
	5 plus parn foo (bar) baz
	5 dash parn foo baz
	5 eqal parn foo baz
	5 qstn parn foo baz
	5 PLUS parn foo baz
	5 DASH parn foo (bar) baz
	5 EQAL parn foo (bar) baz
	5 QSTN parn -> error
	6 plus parn foo (bar) baz
	6 dash parn foo x baz
	6 eqal parn foo x baz
	6 qstn parn foo x baz
	6 PLUS parn foo (bar) baz
	6 DASH parn foo x baz
	6 EQAL parn foo x baz
	6 QSTN parn foo x baz
	7 plus brac foo c } baz
	7 dash brac foo ax{{{}b c d{} baz
	7 eqal brac foo ax{{{}b c ax{{{}b} baz
	7 qstn brac -> error
	7 PLUS brac foo c } baz
	7 DASH brac foo ax{{{}b c d{} baz
	7 EQAL brac foo ax{{{}b c ax{{{}b} baz
	7 QSTN brac -> error
	8 plus brac foo ax{{{}b c d{} baz
	8 dash brac foo c } baz
	8 eqal brac foo c } baz
	8 qstn brac foo c } baz
	8 PLUS brac foo c } baz
	8 DASH brac foo ax{{{}b c d{} baz
	8 EQAL brac foo ax{{{}b c ax{{{}b} baz
	8 QSTN brac -> error
	9 plus brac foo ax{{{}b c d{} baz
	9 dash brac foo x c x} baz
	9 eqal brac foo x c x} baz
	9 qstn brac foo x c x} baz
	9 PLUS brac foo ax{{{}b c d{} baz
	9 DASH brac foo x c x} baz
	9 EQAL brac foo x c x} baz
	9 QSTN brac foo x c x} baz
---

name: expand-weird-1
description:
	Check corner cases of trim expansion vs. $# vs. ${#var} vs. ${var?}
stdin:
	set 1 2 3 4 5 6 7 8 9 10 11
	echo ${#}	# value of $#
	echo ${##}	# length of $#
	echo ${##1}	# $# trimmed 1
	set 1 2 3 4 5 6 7 8 9 10 11 12
	echo ${##1}
	(exit 0)
	echo $? = ${#?} .
	(exit 111)
	echo $? = ${#?} .
expected-stdout:
	11
	2
	1
	2
	0 = 1 .
	111 = 3 .
---

name: expand-weird-3
description:
	Check that trimming works with positional parameters (Debian #48453)
stdin:
	A=9999-02
	B=9999
	echo 1=${A#$B?}.
	set -- $A $B
	echo 2=${1#$2?}.
expected-stdout:
	1=02.
	2=02.
---

name: expand-number-1
description:
	Check that positional arguments do not overflow
stdin:
	echo "1 ${12345678901234567890} ."
expected-stdout:
	1  .
---

name: eglob-subst-1
description:
	Check that eglobbing isn't done on substitution results
file-setup: file 644 "abc"
stdin:
	x='@(*)'
	echo $x
expected-stdout:
	@(*)
---

name: glob-bad-2
description:
	Check that symbolic links aren't stat()'d
# breaks on Dell UNIX 4.0 R2.2 (SVR4) where unlink also fails
# breaks on FreeMiNT (cannot unlink dangling symlinks)
# breaks on MSYS, OS/2 (do not support symlinks)
category: !os:mint,!os:msys,!os:svr4.0,!nosymlink
file-setup: dir 755 "dir"
file-setup: symlink 644 "dir/abc"
	non-existent-file
stdin:
	echo d*/*
	echo d*/abc
expected-stdout:
	dir/abc
	dir/abc
---

name: glob-bad-3
description:
	Check that the slash is parsed before the glob
stdin:
	mkdir a 'a[b'
	(cd 'a[b'; echo ok >'c]d')
	echo nok >abd
	echo fail >a/d
	cat a[b/c]d
expected-stdout:
	ok
---

name: glob-range-1
description:
	Test range matching
file-setup: file 644 ".bc"
file-setup: file 644 "abc"
file-setup: file 644 "bbc"
file-setup: file 644 "cbc"
file-setup: file 644 "-bc"
file-setup: file 644 "!bc"
file-setup: file 644 "^bc"
file-setup: file 644 "+bc"
file-setup: file 644 ",bc"
file-setup: file 644 "0bc"
file-setup: file 644 "1bc"
stdin:
	echo [ab-]*
	echo [-ab]*
	echo [!-ab]*
	echo [!ab]*
	echo [\!ab]*
	echo []ab]*
	echo [^ab]*
	echo [\^ab]*
	echo [+--]*
	echo [--1]*
expected-stdout:
	-bc abc bbc
	-bc abc bbc
	!bc +bc ,bc 0bc 1bc ^bc cbc
	!bc +bc ,bc -bc 0bc 1bc ^bc cbc
	!bc abc bbc
	abc bbc
	^bc abc bbc
	^bc abc bbc
	+bc ,bc -bc
	-bc 0bc 1bc
---

name: glob-range-2
description:
	Test range matching
	(AT&T ksh fails this; POSIX says invalid)
file-setup: file 644 "abc"
stdin:
	echo [a--]*
expected-stdout:
	[a--]*
---

name: glob-range-3
description:
	Check that globbing matches the right things...
# breaks on Mac OSX (HFS+ non-standard UTF-8 canonical decomposition)
# breaks on Cygwin 1.7 (files are now UTF-16 or something)
# breaks on QNX 6.4.1 (says RT)
category: !os:cygwin,!os:midipix,!os:darwin,!os:msys,!os:nto,!os:os2,!os:os390,!noweirdfilenames
need-pass: no
info-pre: test breaks on non-POSIX filesystems, weird locales, etc.
file-setup: file 644 "a�c"
stdin:
	echo a[�-�]*
expected-stdout:
	a�c
---

name: glob-range-4
description:
	Results unspecified according to POSIX
file-setup: file 644 ".bc"
stdin:
	echo [a.]*
expected-stdout:
	[a.]*
---

name: glob-range-5
description:
	Results unspecified according to POSIX
	(AT&T ksh treats this like [a-cc-e]*)
file-setup: file 644 "abc"
file-setup: file 644 "bbc"
file-setup: file 644 "cbc"
file-setup: file 644 "dbc"
file-setup: file 644 "ebc"
file-setup: file 644 "-bc"
file-setup: file 644 "@bc"
stdin:
	echo [a-c-e]*
	echo [a--@]*
expected-stdout:
	-bc abc bbc cbc ebc
	@bc
---

name: glob-range-6
description:
	ksh93 fails this but POSIX probably demands it
file-setup: file 644 "abc"
file-setup: file 644 "cbc"
stdin:
	echo *b*
	[ '*b*' = *b* ] && echo yep; echo $?
expected-stdout:
	abc cbc
	2
expected-stderr-pattern: /.*/
---

name: heredoc-1
description:
	Check ordering/content of redundent here documents.
stdin:
	cat << EOF1 << EOF2
	hi
	EOF1
	there
	EOF2
expected-stdout:
	there
---

name: heredoc-2
description:
	Check quoted here-doc is protected.
stdin:
	a=foo
	cat << 'EOF'
	hi\
	there$a
	stuff
	EO\
	F
	EOF
expected-stdout:
	hi\
	there$a
	stuff
	EO\
	F
---

name: heredoc-3
description:
	Check that newline isn't needed after heredoc-delimiter marker.
stdin: !
	cat << EOF
	hi
	there
	EOF
expected-stdout:
	hi
	there
---

name: heredoc-5
description:
	Check that backslash quotes a $, ` and \ and kills a \newline
stdin:
	a=BAD
	b=ok
	cat << EOF
	h\${a}i
	h\\${b}i
	th\`echo not-run\`ere
	th\\`echo is-run`ere
	fol\\ks
	more\\
	last \
	line
	EOF
expected-stdout:
	h${a}i
	h\oki
	th`echo not-run`ere
	th\is-runere
	fol\ks
	more\
	last line
---

name: heredoc-6
description:
	Check that \newline in initial here-delim word doesn't imply
	a quoted here-doc.
stdin:
	a=i
	cat << EO\
	F
	h$a
	there
	EOF
expected-stdout:
	hi
	there
---

name: heredoc-7
description:
	Check that double quoted $ expressions in here delimiters are
	not expanded and match the delimiter.
	POSIX says only quote removal is applied to the delimiter.
stdin:
	a=b
	cat << "E$a"
	hi
	h$a
	hb
	E$a
	echo done
expected-stdout:
	hi
	h$a
	hb
	done
---

name: heredoc-8
description:
	Check that double quoted escaped $ expressions in here
	delimiters are not expanded and match the delimiter.
	POSIX says only quote removal is applied to the delimiter
	(\ counts as a quote).
stdin:
	a=b
	cat << "E\$a"
	hi
	h$a
	h\$a
	hb
	h\b
	E$a
	echo done
expected-stdout:
	hi
	h$a
	h\$a
	hb
	h\b
	done
---

name: heredoc-15
description:
	Check high-bit7 separators work
stdin:
	u=ä
	tr abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ <<-…
		m${u}h
	…
	echo ok
expected-stdout:
	MäH
	ok
---

name: heredoc-comsub-5
description:
	Check heredoc and COMSUB mixture in input
stdin:
	prefix() { sed -e "s/^/$1:/"; }
	XXX() { echo x-en; }
	YYY() { echo y-es; }
	
	prefix A <<XXX && echo "$(prefix B <<XXX
	echo line 1
	XXX
	echo line 2)" && prefix C <<YYY
	echo line 3
	XXX
	echo line 4)"
	echo line 5
	YYY
	XXX
expected-stdout:
	A:echo line 3
	B:echo line 1
	line 2
	C:echo line 4)"
	C:echo line 5
	x-en
---

name: heredoc-subshell-2
description:
	Tests for here documents in subshells, taken from Austin ML
stdin:
	(cat <<EOF
	some text
	EOF
	)
	echo end
expected-stdout:
	some text
	end
---

name: heredoc-subshell-3
description:
	Tests for here documents in subshells, taken from Austin ML
stdin:
	(cat <<EOF; )
	some text
	EOF
	echo end
expected-stdout:
	some text
	end
---

name: heredoc-weird-1
description:
	Tests for here documents, taken from Austin ML
	Documents current state in mksh, *NOT* necessarily correct!
stdin:
	cat <<END
	hello
	END\
	END
	END
	echo end
expected-stdout:
	hello
	ENDEND
	end
---

name: heredoc-weird-2
description:
	Tests for here documents, taken from Austin ML
stdin:
	cat <<'    END    '
	hello
	    END    
	echo end
expected-stdout:
	hello
	end
---

name: heredoc-weird-4
description:
	Tests for here documents, taken from Austin ML
	Documents current state in mksh, *NOT* necessarily correct!
stdin:
	cat <<END
	hello\
	END
	END
	echo end
expected-stdout:
	helloEND
	end
---

name: heredoc-weird-5
description:
	Tests for here documents, taken from Austin ML
	Documents current state in mksh, *NOT* necessarily correct!
stdin:
	cat <<END
	hello
	\END
	END
	echo end
expected-stdout:
	hello
	\END
	end
---

name: heredoc-quoting-unsubst
description:
	Check for correct handling of quoted characters in
	here documents without substitution (marker is quoted).
stdin:
	foo=bar
	cat <<-'EOF'
		x " \" \ \\ $ \$ `echo baz` \`echo baz\` $foo \$foo x
	EOF
expected-stdout:
	x " \" \ \\ $ \$ `echo baz` \`echo baz\` $foo \$foo x
---

name: heredoc-quoting-subst
description:
	Check for correct handling of quoted characters in
	here documents with substitution (marker is not quoted).
stdin:
	foo=bar
	cat <<-EOF
		x " \" \ \\ $ \$ `echo baz` \`echo baz\` $foo \$foo x
	EOF
expected-stdout:
	x " \" \ \ $ $ baz `echo baz` bar $foo x
---

name: single-quotes-in-braces
description:
	Check that single quotes inside unquoted {} are treated as quotes
stdin:
	foo=1
	echo ${foo:+'blah  $foo'}
expected-stdout:
	blah  $foo
---

name: single-quotes-in-quoted-braces
description:
	Check that single quotes inside quoted {} are treated as
	normal char
stdin:
	foo=1
	echo "${foo:+'blah  $foo'}"
expected-stdout:
	'blah  1'
---

name: single-quotes-in-braces-nested
description:
	Check that single quotes inside unquoted {} are treated as quotes,
	even if that's inside a double-quoted command expansion
stdin:
	foo=1
	echo "$( echo ${foo:+'blah  $foo'})"
expected-stdout:
	blah  $foo
---

name: single-quotes-in-brace-pattern
description:
	Check that single quotes inside {} pattern are treated as quotes
stdin:
	foo=1234
	echo ${foo%'2'*} "${foo%'2'*}" ${foo%2'*'} "${foo%2'*'}"
expected-stdout:
	1 1 1234 1234
---

name: single-quotes-in-heredoc-braces
description:
	Check that single quotes inside {} in heredoc are treated
	as normal char
stdin:
	foo=1
	cat <<EOM
	${foo:+'blah  $foo'}
	EOM
expected-stdout:
	'blah  1'
---

name: single-quotes-in-nested-braces
description:
	Check that single quotes inside nested unquoted {} are
	treated as quotes
stdin:
	foo=1
	echo ${foo:+${foo:+'blah  $foo'}}
expected-stdout:
	blah  $foo
---

name: single-quotes-in-nested-quoted-braces
description:
	Check that single quotes inside nested quoted {} are treated
	as normal char
stdin:
	foo=1
	echo "${foo:+${foo:+'blah  $foo'}}"
expected-stdout:
	'blah  1'
---

name: single-quotes-in-nested-braces-nested
description:
	Check that single quotes inside nested unquoted {} are treated
	as quotes, even if that's inside a double-quoted command expansion
stdin:
	foo=1
	echo "$( echo ${foo:+${foo:+'blah  $foo'}})"
expected-stdout:
	blah  $foo
---

name: single-quotes-in-nested-brace-pattern
description:
	Check that single quotes inside nested {} pattern are treated as quotes
stdin:
	foo=1234
	echo ${foo:+${foo%'2'*}} "${foo:+${foo%'2'*}}" ${foo:+${foo%2'*'}} "${foo:+${foo%2'*'}}"
expected-stdout:
	1 1 1234 1234
---

name: single-quotes-in-heredoc-nested-braces
description:
	Check that single quotes inside nested {} in heredoc are treated
	as normal char
stdin:
	foo=1
	cat <<EOM
	${foo:+${foo:+'blah  $foo'}}
	EOM
expected-stdout:
	'blah  1'
---

name: IFS-space-1
description:
	Simple test, default IFS
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	set -- A B C
	showargs 1 $*
	showargs 2 "$*"
	showargs 3 $@
	showargs 4 "$@"
expected-stdout:
	<1> <A> <B> <C> .
	<2> <A B C> .
	<3> <A> <B> <C> .
	<4> <A> <B> <C> .
---

name: IFS-colon-1
description:
	Simple test, IFS=:
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS=:
	set -- A B C
	showargs 1 $*
	showargs 2 "$*"
	showargs 3 $@
	showargs 4 "$@"
expected-stdout:
	<1> <A> <B> <C> .
	<2> <A:B:C> .
	<3> <A> <B> <C> .
	<4> <A> <B> <C> .
---

name: IFS-null-1
description:
	Simple test, IFS=""
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS=""
	set -- A B C
	showargs 1 $*
	showargs 2 "$*"
	showargs 3 $@
	showargs 4 "$@"
expected-stdout:
	<1> <A> <B> <C> .
	<2> <ABC> .
	<3> <A> <B> <C> .
	<4> <A> <B> <C> .
---

name: IFS-space-colon-1
description:
	Simple test, IFS=<white-space>:
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS="$IFS:"
	set --
	showargs 1 $*
	showargs 2 "$*"
	showargs 3 $@
	showargs 4 "$@"
	showargs 5 : "$@"
expected-stdout:
	<1> .
	<2> <> .
	<3> .
	<4> .
	<5> <:> .
---

name: IFS-space-colon-2
description:
	Simple test, IFS=<white-space>:
	AT&T ksh fails this, POSIX says the test is correct.
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS="$IFS:"
	set --
	showargs :"$@"
expected-stdout:
	<:> .
---

name: IFS-space-colon-5
description:
	Simple test, IFS=<white-space>:
	Don't know what POSIX thinks of this.  AT&T ksh does not do this.
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS="$IFS:"
	set --
	showargs "${@:-}"
expected-stdout:
	<> .
---

name: IFS-subst-1
description:
	Simple test, IFS=<white-space>:
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS="$IFS:"
	x=":b: :"
	echo -n '1:'; for i in $x ; do echo -n " [$i]" ; done ; echo
	echo -n '2:'; for i in :b:: ; do echo -n " [$i]" ; done ; echo
	showargs 3 $x
	showargs 4 :b::
	x="a:b:"
	echo -n '5:'; for i in $x ; do echo -n " [$i]" ; done ; echo
	showargs 6 $x
	x="a::c"
	echo -n '7:'; for i in $x ; do echo -n " [$i]" ; done ; echo
	showargs 8 $x
	echo -n '9:'; for i in ${FOO-`echo -n h:i`th:ere} ; do echo -n " [$i]" ; done ; echo
	showargs 10 ${FOO-`echo -n h:i`th:ere}
	showargs 11 "${FOO-`echo -n h:i`th:ere}"
	x=" A :  B::D"
	echo -n '12:'; for i in $x ; do echo -n " [$i]" ; done ; echo
	showargs 13 $x
expected-stdout:
	1: [] [b] []
	2: [:b::]
	<3> <> <b> <> .
	<4> <:b::> .
	5: [a] [b]
	<6> <a> <b> .
	7: [a] [] [c]
	<8> <a> <> <c> .
	9: [h] [ith] [ere]
	<10> <h> <ith> <ere> .
	<11> <h:ith:ere> .
	12: [A] [B] [] [D]
	<13> <A> <B> <> <D> .
---

name: IFS-subst-2
description:
	Check leading whitespace after trim does not make a field
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	x="X 1 2"
	showargs 1 shift ${x#X}
expected-stdout:
	<1> <shift> <1> <2> .
---

name: IFS-subst-3-arr
description:
	Check leading IFS non-whitespace after trim does make a field
	but leading IFS whitespace does not, nor empty replacements
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	showargs 0 ${-+}
	IFS=:
	showargs 1 ${-+:foo:bar}
	IFS=' '
	showargs 2 ${-+ foo bar}
expected-stdout:
	<0> .
	<1> <> <foo> <bar> .
	<2> <foo> <bar> .
---

name: IFS-subst-3-ass
description:
	Check non-field semantics
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	showargs 0 x=${-+}
	IFS=:
	showargs 1 x=${-+:foo:bar}
	IFS=' '
	showargs 2 x=${-+ foo bar}
expected-stdout:
	<0> <x=> .
	<1> <x=> <foo> <bar> .
	<2> <x=> <foo> <bar> .
---

name: IFS-subst-3-lcl
description:
	Check non-field semantics, smaller corner case (LP#1381965)
stdin:
	set -x
	local regex=${2:-}
	exit 1
expected-exit: e != 0
expected-stderr-pattern:
	/regex=/
---

name: IFS-subst-6
description:
	Regression wrt. vector expansion in trim
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS=
	x=abc
	set -- a b
	showargs ${x#$*}
expected-stdout:
	<c> .
---

name: IFS-subst-7
description:
	ksh93 bug wrt. vector expansion in trim
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	IFS="*"
	a=abcd
	set -- '' c
	showargs "$*" ${a##"$*"}
expected-stdout:
	<*c> <abcd> .
---

name: IFS-subst-8
description:
	https://www.austingroupbugs.net/view.php?id=221
stdin:
	n() { echo "$#"; }; n "${foo-$@}"
expected-stdout:
	1
---

name: IFS-subst-10
description:
	Scalar context in ${var=$subst}
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	set -- one "two three" four
	unset -v var
	save_IFS=$IFS
	IFS=
	set -- ${var=$*}
	IFS=$save_IFS
	echo "var=$var"
	showargs "$@"
expected-stdout:
	var=onetwo threefour
	<onetwo threefour> .
---

name: integer-arithmetic-span-signed
description:
	Check wraparound and size that is defined in mksh
category: shell:legacy-no
stdin:
	echo s:$((2147483647+1)).$(((2147483647*2)+1)).$(((2147483647*2)+2)).
	echo u:$((#2147483647+1)).$((#(2147483647*2)+1)).$((#(2147483647*2)+2)).
expected-stdout:
	s:-2147483648.-1.0.
	u:2147483648.4294967295.0.
---

name: integer-arithmetic-span-32
description:
	Check unsigned wraparound and size that should also work in lksh
category: int:32
stdin:
	echo u:$((#2147483647+1)).$((#(2147483647*2)+1)).$((#(2147483647*2)+2)).
expected-stdout:
	u:2147483648.4294967295.0.
---

name: integer-arithmetic-span-64
description:
	Check unsigned wraparound and size that should also work in lksh
category: int:64
stdin:
	echo u:$((#9223372036854775807+1)).$((#(9223372036854775807*2)+1)).$((#(9223372036854775807*2)+2)).
expected-stdout:
	u:9223372036854775808.18446744073709551615.0.
---

name: integer-size-FAIL-to-detect
description:
	Notify the user that their ints are not 32 or 64 bit
category: int:u
stdin:
	:
---

name: read-IFS-1
description:
	Simple test, default IFS
stdin:
	echo "A B " > IN
	unset x y z
	read x y z < IN
	echo 1: "x[$x] y[$y] z[$z]"
	echo 1a: ${z-z not set}
	read x < IN
	echo 2: "x[$x]"
expected-stdout:
	1: x[A] y[B] z[]
	1a:
	2: x[A B]
---

name: read-regress-1
description:
	Check a regression of read
file-setup: file 644 "foo"
	foo bar
	baz
	blah
stdin:
	while read a b c; do
		read d
		break
	done <foo
	echo "<$a|$b|$c><$d>"
expected-stdout:
	<foo|bar|><baz>
---

name: regression-1
description:
	Lex array code had problems with this.
stdin:
	echo foo[
	n=bar
	echo "hi[ $n ]=1"
expected-stdout:
	foo[
	hi[ bar ]=1
---

name: regression-6
description:
	Parsing of $(..) expressions is non-optimal.  It is
	impossible to have any parentheses inside the expression.
	I.e.,
		$ ksh -c 'echo $(echo \( )'
		no closing quote
		$ ksh -c 'echo $(echo "(" )'
		no closing quote
		$
	The solution is to hack the parsing clode in lex.c, the
	question is how to hack it: should any parentheses be
	escaped by a backslash, or should recursive parsing be done
	(so quotes could also be used to hide hem).  The former is
	easier, the later better...
stdin:
	echo $(echo \( )
	echo $(echo "(" )
expected-stdout:
	(
	(
---

name: regression-9
description:
	Continue in a for loop does not work right:
		for i in a b c ; do
			if [ $i = b ] ; then
				continue
			fi
			echo $i
		done
	Prints a forever...
stdin:
	first=yes
	for i in a b c ; do
		if [ $i = b ] ; then
			if [ $first = no ] ; then
				echo 'continue in for loop broken'
				break	# hope break isn't broken too :-)
			fi
			first=no
			continue
		fi
	done
	echo bye
expected-stdout:
	bye
---

name: regression-12
description:
	Both of the following echos produce the same output under sh/ksh.att:
		#!/bin/sh
		x="foo	bar"
		echo "`echo \"$x\"`"
		echo "`echo "$x"`"
	pdksh produces different output for the former (foo instead of foo\tbar)
stdin:
	x="foo	bar"
	echo "`echo \"$x\"`"
	echo "`echo "$x"`"
expected-stdout:
	foo	bar
	foo	bar
---

name: regression-13
description:
	The following command hangs forever:
		$ (: ; cat /etc/termcap) | sleep 2
	This is because the shell forks a shell to run the (..) command
	and this shell has the pipe open.  When the sleep dies, the cat
	doesn't get a SIGPIPE 'cause a process (ie, the second shell)
	still has the pipe open.
	
	NOTE: this test provokes a bizarre bug in ksh93 (shell starts reading
	      commands from /etc/termcap..)
time-limit: 10
stdin:
	echo A line of text that will be duplicated quite a number of times.> t1
	cat t1 t1 t1 t1  t1 t1 t1 t1  t1 t1 t1 t1  t1 t1 t1 t1  > t2
	cat t2 t2 t2 t2  t2 t2 t2 t2  t2 t2 t2 t2  t2 t2 t2 t2  > t1
	cat t1 t1 t1 t1 > t2
	(: ; cat t2 2>/dev/null) | sleep 1
---

name: regression-14
description:
	The command
		$ (foobar) 2> /dev/null
	generates no output under /bin/sh, but pdksh produces the error
		foobar: not found
	Also, the command
		$ foobar 2> /dev/null
	generates an error under /bin/sh and pdksh, but AT&T ksh88 produces
	no error (redirected to /dev/null).
stdin:
	(you/should/not/see/this/error/1) 2> /dev/null
	you/should/not/see/this/error/2 2> /dev/null
	true
---

name: regression-16
description:
	${var%%expr} seems to be broken in many places.  On the mips
	the commands
		$ read line < /etc/passwd
		$ echo $line
		root:0:1:...
		$ echo ${line%%:*}
		root
		$ echo $line
		root
		$
	change the value of line.  On sun4s & pas, the echo ${line%%:*} doesn't
	work.  Haven't checked elsewhere...
script:
	read x
	y=$x
	echo ${x%%:*}
	echo $x
stdin:
	root:asdjhasdasjhs:0:1:Root:/:/bin/sh
expected-stdout:
	root
	root:asdjhasdasjhs:0:1:Root:/:/bin/sh
---

name: regression-17
description:
	The command
		. /foo/bar
	should set the exit status to non-zero (sh and AT&T ksh88 do).
	XXX doting a non existent file is a fatal error for a script
stdin:
	. does/not/exist
expected-exit: e != 0
expected-stderr-pattern: /.?/
---

name: regression-21
description:
	backslash does not work as expected in case labels:
	$ x='-x'
	$ case $x in
	-\?) echo hi
	esac
	hi
	$ x='-?'
	$ case $x in
	-\\?) echo hi
	esac
	hi
	$
stdin:
	case -x in
	-\?)	echo fail
	esac
---

name: regression-22
description:
	Quoting backquotes inside backquotes doesn't work:
	$ echo `echo hi \`echo there\` folks`
	asks for more info.  sh and AT&T ksh88 both echo
	hi there folks
stdin:
	echo `echo hi \`echo there\` folks`
expected-stdout:
	hi there folks
---

name: regression-23
description:
	)) is not treated `correctly':
	    $ (echo hi ; (echo there ; echo folks))
	    missing ((
	    $
	instead of (as sh and ksh.att)
	    $ (echo hi ; (echo there ; echo folks))
	    hi
	    there
	    folks
	    $
stdin:
	( : ; ( : ; echo hi))
expected-stdout:
	hi
---

name: regression-25
description:
	Check reading stdin in a while loop.  The read should only read
	a single line, not a whole stdio buffer; the cat should get
	the rest.
stdin:
	(echo a; echo b) | while read x ; do
	    echo $x
	    cat > /dev/null
	done
expected-stdout:
	a
---

name: regression-26
description:
	Check reading stdin in a while loop.  The read should read both
	lines, not just the first.
script:
	a=
	while [ "$a" != xxx ] ; do
	    last=$x
	    read x
	    cat /dev/null | sed 's/x/y/'
	    a=x$a
	done
	echo $last
stdin:
	a
	b
expected-stdout:
	b
---

name: regression-27
description:
	The command
		. /does/not/exist
	should cause a script to exit.
stdin:
	. does/not/exist
	echo hi
expected-exit: e != 0
expected-stderr-pattern: /does\/not\/exist/
---

name: regression-28
description:
	variable assignments not detected well
stdin:
	a.x=1 echo hi
expected-exit: e != 0
expected-stderr-pattern: /a\.x=1/
---

name: regression-30
description:
	strange characters allowed inside ${...}
stdin:
	echo ${a{b}}
expected-exit: e != 0
expected-stderr-pattern: /.?/
---

name: regression-31
description:
	Does read handle partial lines correctly
script:
	a= ret=
	while [ "$a" != xxx ] ; do
	    read x y z
	    ret=$?
	    a=x$a
	done
	echo "[$x]"
	echo $ret
stdin: !
	a A aA
	b B Bb
	c
expected-stdout:
	[c]
	1
---

name: regression-32
description:
	Does read set variables to null at eof?
script:
	a=
	while [ "$a" != xxx ] ; do
	    read x y z
	    a=x$a
	done
	echo 1: ${x-x not set} ${y-y not set} ${z-z not set}
	echo 2: ${x:+x not null} ${y:+y not null} ${z:+z not null}
stdin:
	a A Aa
	b B Bb
expected-stdout:
	1:
	2:
---

name: regression-33
description:
	Does umask print a leading 0 when umask is 3 digits?
# prints 0600…
category: !os:skyos
stdin:
	# on MiNT, the first umask call seems to fail
	umask 022
	# now, the test proper
	umask 222
	umask
expected-stdout:
	0222
---

name: regression-35
description:
	Temporary files used for heredocs in functions get trashed after
	the function is parsed (before it is executed)
stdin:
	f1() {
		cat <<- EOF
			F1
		EOF
		f2() {
			cat <<- EOF
				F2
			EOF
		}
	}
	f1
	f2
	unset -f f1
	f2
expected-stdout:
	F1
	F2
	F2
---

name: regression-36
description:
	Command substitution breaks reading in while loop
	(test from <sjg@void.zen.oz.au>)
stdin:
	(echo abcdef; echo; echo 123) |
	    while read line
	    do
	      # the following line breaks it
	      c=`echo $line | wc -c`
	      echo $c
	    done
expected-stdout:
	7
	1
	4
---

name: regression-37
description:
	Machines with broken times() (reported by <sjg@void.zen.oz.au>)
	time does not report correct real time
stdin:
	time -p sleep 1
expected-stderr-pattern: /^real +(?![0.]*$)[0-9]+(?:\.[0-9]+)?$/m
---

name: regression-38
description:
	set -e doesn't ignore exit codes for if/while/until/&&/||/!.
arguments: !-e!
stdin:
	if false; then echo hi ; fi
	false || true
	false && true
	while false; do echo hi; done
	echo ok
expected-stdout:
	ok
---

name: regression-39
description:
	Only posh and oksh(2013-07) say “hi” below; FreeBSD sh,
	GNU bash in POSIX mode, dash, ksh93, mksh don’t. All of
	them exit 0. The POSIX behaviour is needed by BSD make.
stdin:
	set -e
	echo `false; echo hi` $(<this-file-does-not-exist)
	echo $?
expected-stdout:
	
	0
expected-stderr-pattern: /this-file-does-not-exist/
---

name: regression-40
description:
	This used to cause a core dump
env-setup: !RANDOM=12!
stdin:
	echo hi
expected-stdout:
	hi
---

name: regression-41
description:
	foo should be set to bar (should not be empty)
stdin:
	foo=`
	echo bar`
	echo "($foo)"
expected-stdout:
	(bar)
---

name: regression-45
description:
	Parameter assignments with [] recognised correctly
stdin:
	FOO=*[12]
	BAR=abc[
	MORE=[abc]
	JUNK=a[bc
	echo "<$FOO>"
	echo "<$BAR>"
	echo "<$MORE>"
	echo "<$JUNK>"
expected-stdout:
	<*[12]>
	<abc[>
	<[abc]>
	<a[bc>
---

name: regression-58
description:
	Check if trap exit is ok (exit not mistaken for signal name)
stdin:
	trap 'echo hi' exit
	trap exit 1
expected-stdout:
	hi
---

name: regression-60
description:
	Check if default exit status is previous command
stdin:
	(true; exit)
	echo A $?
	(false; exit)
	echo B $?
	( (exit 103) ; exit)
	echo C $?
expected-stdout:
	A 0
	B 1
	C 103
---

name: regression-61
description:
	Check if EXIT trap is executed for sub shells.
stdin:
	trap 'echo parent exit' EXIT
	echo start
	(echo A; echo A last)
	echo B
	(echo C; trap 'echo sub exit' EXIT; echo C last)
	echo parent last
expected-stdout:
	start
	A
	A last
	B
	C
	C last
	sub exit
	parent last
	parent exit
---

name: regression-64
description:
	Check that we can redefine functions calling time builtin
stdin:
	t() {
		time >/dev/null
	}
	t 2>/dev/null
	t() {
		time
	}
---

name: regression-68-nolksh
description:
	Things POSIX/C arithmetics don’t guarantee
category: shell:legacy-no
stdin:
	echo '1(26)' , $(( 5 % -1 )) , $(( 5 % -2 )) .
expected-stdout:
	1(26) , 0 , 1 .
---

name: regression-69
description:
	Check that all non-lksh arithmetic operators work as expected
category: shell:legacy-no
stdin:
	a=5 b=0x80000005
	echo 1 $(( a ^<= 1 )) , $(( b ^<= 1 )) .
	echo 2 $(( a ^>= 2 )) , $(( b ^>= 2 )) .
	echo 3 $(( 5 ^< 1 )) , $(( 5 ^< 0 )) .
	echo 4 $(( 5 ^> 1 )) , $((# 5 ^> 1 )) .
	echo 5 $(( 5 ^> 0 )) , $((# 5 ^> 0 )) .
expected-stdout:
	1 10 , 11 .
	2 -2147483646 , -1073741822 .
	3 10 , 5 .
	4 -2147483646 , 2147483650 .
	5 5 , 5 .
---

name: readonly-5
description:
	Ensure readonly is idempotent
stdin:
	readonly x=1
	readonly x
---

name: xxx-quoted-newline-1
description:
	Check that \<newline> works inside of ${}
stdin:
	abc=2
	echo ${ab\
	c}
expected-stdout:
	2
---

name: xxx-quoted-newline-2
description:
	Check that \<newline> works at the start of a here document
stdin:
	cat << EO\
	F
	hi
	EOF
expected-stdout:
	hi
---

name: xxx-multi-assignment-cmd
description:
	Check that assignments in a command affect subsequent assignments
	in the same command
stdin:
	FOO=abc
	FOO=123 BAR=$FOO
	echo $BAR
expected-stdout:
	123
---

name: xxx-multi-assignment-posix-nocmd
description:
	Check that the behaviour for multiple assignments with no
	command name matches POSIX (Debian #334182). See:
	http://thread.gmane.org/gmane.comp.standards.posix.austin.general/1925
stdin:
	X=a Y=b; X=$Y Y=$X; echo 1 $X $Y .
expected-stdout:
	1 b b .
---

name: exec-function-environment-1
description:
	Check assignments in function calls and whether they affect
	the current execution environment
stdin:
	f() { a=2; }; g() { b=3; echo y$c-; }; a=1 f; b=2; c=1 g
	echo x$a-$b- z$c-
expected-stdout:
	y1-
	x-3- z-
---

name: xxx-what-do-you-call-this-1
stdin:
	echo "${foo:-"a"}*"
expected-stdout:
	a*
---

name: xxx-prefix-strip-1
stdin:
	foo='a cdef'
	echo ${foo#a c}
expected-stdout:
	def
---

name: xxx-prefix-strip-2
stdin:
	set -- a c
	x='a cdef'
	echo ${x#$*}
expected-stdout:
	def
---

name: xxx-variable-syntax-4
description:
	Not all kinds of trims are currently impossible, check those who do
stdin:
	foo() {
		echo "<$*> X${*:+ }X"
	}
	foo a b
	foo "" c
	foo ""
	foo "" ""
	IFS=:
	foo a b
	foo "" c
	foo ""
	foo "" ""
	IFS=
	foo a b
	foo "" c
	foo ""
	foo "" ""
expected-stdout:
	<a b> X X
	< c> X X
	<> XX
	< > X X
	<a:b> X X
	<:c> X X
	<> XX
	<:> X X
	<ab> X X
	<c> X X
	<> XX
	<> XX
---

name: xxx-while-1
description:
	Check the return value of while loops
	XXX need to do same for for/select/until loops
stdin:
	i=x
	while [ $i != xxx ] ; do
	    i=x$i
	    if [ $i = xxx ] ; then
		false
		continue
	    fi
	done
	echo loop1=$?
	
	i=x
	while [ $i != xxx ] ; do
	    i=x$i
	    if [ $i = xxx ] ; then
		false
		break
	    fi
	done
	echo loop2=$?
	
	i=x
	while [ $i != xxx ] ; do
	    i=x$i
	    false
	done
	echo loop3=$?
expected-stdout:
	loop1=0
	loop2=0
	loop3=1
---

name: xxx-clean-chars-1
description:
	Check MAGIC character is stuffed correctly
stdin:
	echo `echo [�`
expected-stdout:
	[�
---

name: exit-err-4
description:
	"set -e" test suite (POSIX)
stdin:
	set -e
	echo pre
	if true ; then
		false && echo foo
	fi
	echo bar
expected-stdout:
	pre
	bar
---

name: exit-err-7
description:
	"set -e" regression (LP#1104543)
stdin:
	set -e
	bla() {
		[ -x $PWD/nonexistant ] && $PWD/nonexistant
	}
	echo x
	bla
	echo y$?
expected-stdout:
	x
expected-exit: 1
---

name: exit-err-8
description:
	"set -e" regression (Debian #700526)
stdin:
	set -e
	_db_cmd() { return $1; }
	db_input() { _db_cmd 30; }
	db_go() { _db_cmd 0; }
	db_input || :
	db_go
	exit 0
---

name: exit-err-9
description:
	"set -e" versus bang pipelines
stdin:
	set -e
	! false | false
	echo 1 ok
	! false && false
	echo 2 wrong
expected-stdout:
	1 ok
expected-exit: 1
---

name: exit-err-10
description:
	Debian #269067 (cf. regression-38 but with eval)
arguments: !-e!
stdin:
	eval false || true
	echo = $? .
expected-stdout:
	= 0 .
---

name: exit-eval-1
description:
	Check eval vs substitution exit codes (ksh93 alike)
stdin:
	(exit 12)
	eval $(false)
	echo A $?
	(exit 12)
	eval ' $(false)'
	echo B $?
	(exit 12)
	eval " $(false)"
	echo C $?
	(exit 12)
	eval "eval $(false)"
	echo D $?
	(exit 12)
	eval 'eval '"$(false)"
	echo E $?
	IFS="$IFS:"
	(exit 12)
	eval $(echo :; false)
	echo F $?
	echo -n "G "
	(exit 12)
	eval 'echo $?'
	echo H $?
expected-stdout:
	A 0
	B 1
	C 0
	D 0
	E 0
	F 0
	G 12
	H 0
---

name: exit-trap-1
description:
	Check that "exit" with no arguments behaves SUSv4 conformant.
stdin:
	trap 'echo hi; exit' EXIT
	exit 9
expected-stdout:
	hi
expected-exit: 9
---

name: exit-trap-3
description:
	Check that the EXIT trap is run in many places, Debian #910276
stdin:
	fkt() {
		trap -- "echo $1 >&2" EXIT
	}
	fkt shell_exit
	$(fkt fn_exit)
	$(trap -- "echo comsub_exit >&2" EXIT)
	(trap -- "echo subshell_exit >&2" EXIT)
expected-stderr:
	fn_exit
	comsub_exit
	subshell_exit
	shell_exit
---

name: test-stlt-1
description:
	Check that test also can handle string1 < string2 etc.
stdin:
	test 2005/10/08 '<' 2005/08/21 && echo ja || echo nein
	test 2005/08/21 \< 2005/10/08 && echo ja || echo nein
	test 2005/10/08 '>' 2005/08/21 && echo ja || echo nein
	test 2005/08/21 \> 2005/10/08 && echo ja || echo nein
expected-stdout:
	nein
	ja
	ja
	nein
expected-stderr-pattern: !/unexpected op/
---

name: test-precedence-1
description:
	Check a weird precedence case (and POSIX echo)
stdin:
	test \( -f = -f \)
	rv=$?
	echo $rv
expected-stdout:
	0
---

name: mkshrc-1
description:
	Check that ~/.mkshrc works correctly.
	Part 1: verify user environment is not read (internal)
stdin:
	echo x $FNORD
expected-stdout:
	x
---

name: mkshrc-2a
description:
	Check that ~/.mkshrc works correctly.
	Part 2: verify mkshrc is not read (non-interactive shells)
file-setup: file 644 ".mkshrc"
	FNORD=42
env-setup: !HOME=.!ENV=!
stdin:
	echo x $FNORD
expected-stdout:
	x
---

name: mkshrc-3
description:
	Check that ~/.mkshrc works correctly.
	Part 3: verify mkshrc can be turned off
file-setup: file 644 ".mkshrc"
	FNORD=42
env-setup: !HOME=.!ENV=nonexistant!
stdin:
	echo x $FNORD
expected-stdout:
	x
---

name: varexpand-null-3
description:
	Ensure concatenating behaviour matches other shells
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	showargs 0 ""$@
	x=; showargs 1 "$x"$@
	set A; showargs 2 "${@:+}"
	n() { echo "$#"; }
	unset e
	set -- a b
	n """$@"
	n "$@"
	n "$@"""
	n "$e""$@"
	n "$@"
	n "$@""$e"
	set --
	n """$@"
	n "$@"
	n "$@"""
	n "$e""$@"
	n "$@"
	n "$@""$e"
expected-stdout:
	<0> <> .
	<1> <> .
	<2> <> .
	2
	2
	2
	2
	2
	2
	1
	0
	1
	1
	0
	1
---

name: dot-errorlevel
description:
	Ensure dot resets $?
stdin:
	:>dotfile
	(exit 42)
	. ./dotfile
	echo 1 $? .
expected-stdout:
	1 0 .
---

name: oksh-eval
description:
	Check expansions.
stdin:
	a=
	for n in ${a#*=}; do echo 1hu ${n} .; done
	for n in "${a#*=}"; do echo 1hq ${n} .; done
	for n in ${a##*=}; do echo 2hu ${n} .; done
	for n in "${a##*=}"; do echo 2hq ${n} .; done
	for n in ${a%=*}; do echo 1pu ${n} .; done
	for n in "${a%=*}"; do echo 1pq ${n} .; done
	for n in ${a%%=*}; do echo 2pu ${n} .; done
	for n in "${a%%=*}"; do echo 2pq ${n} .; done
expected-stdout:
	1hq .
	2hq .
	1pq .
	2pq .
---

name: oksh-and-list-error-1
description:
	Test exit status of rightmost element in 2 element && list in -e mode
stdin:
	true && false
	echo "should not print"
arguments: !-e!
expected-exit: e != 0
---

name: oksh-and-list-error-2
description:
	Test exit status of rightmost element in 3 element && list in -e mode
stdin:
	true && true && false
	echo "should not print"
arguments: !-e!
expected-exit: e != 0
---

name: oksh-or-list-error-1
description:
	Test exit status of || list in -e mode
stdin:
	false || false
	echo "should not print"
arguments: !-e!
expected-exit: e != 0
---

name: oksh-seterror-1
description:
	The -e flag should be ignored when executing a compound list
	followed by an if statement.
stdin:
	if true; then false && false; fi
	true
arguments: !-e!
expected-exit: e == 0
---

name: oksh-seterror-2
description:
	The -e flag should be ignored when executing a compound list
	followed by an if statement.
stdin:
	if true; then if true; then false && false; fi; fi
	true
arguments: !-e!
expected-exit: e == 0
---

name: oksh-seterror-3
description:
	The -e flag should be ignored when executing a compound list
	followed by an elif statement.
stdin:
	if true; then :; elif true; then false && false; fi
arguments: !-e!
expected-exit: e == 0
---

name: oksh-seterror-4
description:
	The -e flag should be ignored when executing a pipeline
	beginning with '!'
stdin:
	for i in 1 2 3
	do
		false && false
		true || false
	done
arguments: !-e!
expected-exit: e == 0
---

name: oksh-seterror-5
description:
	The -e flag should be ignored when executing a pipeline
	beginning with '!'
stdin:
	! true | false
	true
arguments: !-e!
expected-exit: e == 0
---

name: oksh-seterror-7
description:
	The -e flag within a command substitution should be honored
stdin:
	echo $( set -e; false; echo foo )
arguments: !-e!
expected-stdout:
	
---

name: oksh-input-comsub
description:
	A command substitution using input redirection should exit with
	failure if the input file does not exist.
stdin:
	var=$(< non-existent)
expected-exit: e != 0
expected-stderr-pattern: /non-existent/
---

name: oksh-empty-for-list
description:
	A for list which expands to zero items should not execute the body.
stdin:
	set foo bar baz ; for out in ; do echo $out ; done
---

name: for-without-list
description:
	LP#2002250
stdin:
	set -- a b
	for x
	do
		echo $x
		shift $#
	done
expected-stdout:
	a
	b
---

name: comsub-2
description:
	RedHat BZ#496791 – another case of missing recursion
	in parsing COMSUB expressions
	Fails on: pdksh bash2 bash3¹ bash4¹ zsh
	Passes on: ksh93 mksh(20110305+)
	① bash[34] seem to choke on comment ending with backslash-newline
stdin:
	# a comment with " ' \
	x=$(
	echo yes
	# a comment with " ' \
	)
	echo $x
expected-stdout:
	yes
---

name: better-parens-1a
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	if ( (echo fubar)|tr u x); then
		echo ja
	else
		echo nein
	fi
expected-stdout:
	fxbar
	ja
---

name: better-parens-1b
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	echo $( (echo fubar)|tr u x) $?
expected-stdout:
	fxbar 0
---

name: better-parens-1c
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	x=$( (echo fubar)|tr u x); echo $x $?
expected-stdout:
	fxbar 0
---

name: better-parens-2a
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	if ((echo fubar)|tr u x); then
		echo ja
	else
		echo nein
	fi
expected-stdout:
	fxbar
	ja
---

name: better-parens-3a
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	if ( (echo fubar)|(tr u x)); then
		echo ja
	else
		echo nein
	fi
expected-stdout:
	fxbar
	ja
---

name: better-parens-3b
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	echo $( (echo fubar)|(tr u x)) $?
expected-stdout:
	fxbar 0
---

name: better-parens-3c
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	x=$( (echo fubar)|(tr u x)); echo $x $?
expected-stdout:
	fxbar 0
---

name: better-parens-4a
description:
	Check support for ((…)) and $((…)) vs (…) and $(…)
stdin:
	if ((echo fubar)|(tr u x)); then
		echo ja
	else
		echo nein
	fi
expected-stdout:
	fxbar
	ja
---

name: better-parens-5
description:
	Another corner case
stdin:
	( (echo 'fo	o$bar' "baz\$bla\"" m\$eh) | tr a A)
	((echo 'fo	o$bar' "baz\$bla\"" m\$eh) | tr a A)
expected-stdout:
	fo	o$bAr bAz$blA" m$eh
	fo	o$bAr bAz$blA" m$eh
---

name: utilities-getopts-1
description:
	getopts sets OPTIND correctly for unparsed option
stdin:
	set -- -a -a -x
	while getopts :a optc; do
	    echo "OPTARG=$OPTARG, OPTIND=$OPTIND, optc=$optc."
	done
	echo done
expected-stdout:
	OPTARG=, OPTIND=2, optc=a.
	OPTARG=, OPTIND=3, optc=a.
	OPTARG=x, OPTIND=4, optc=?.
	done
---

name: debian-117-1
description:
	Check test - bug#465250
stdin:
	test \( ! -e \) ; echo $?
expected-stdout:
	1
---

name: debian-117-2
description:
	Check test - bug#465250
stdin:
	test \(  -e \) ; echo $?
expected-stdout:
	0
---

name: debian-117-3
description:
	Check test - bug#465250
stdin:
	test ! -e  ; echo $?
expected-stdout:
	1
---

name: debian-117-4
description:
	Check test - bug#465250
stdin:
	test  -e  ; echo $?
expected-stdout:
	0
---

name: command-set
description:
	Same but with set
stdin:
	showargs() { for s_arg in "$@"; do echo -n "<$s_arg> "; done; echo .; }
	showargs 1 "$@"
	set -- foo bar baz
	showargs 2 "$@"
	command set -- miau 'meow nyao'
	showargs 3 "$@"
expected-stdout:
	<1> .
	<2> <foo> <bar> <baz> .
	<3> <miau> <meow nyao> .
---
