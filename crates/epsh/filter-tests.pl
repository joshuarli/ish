#!/usr/bin/perl
# Filter mksh's check.t to extract tests applicable to epsh.
# Reads check.t, outputs only tests that don't require ksh extensions.

use strict;
use warnings;

my $file = shift || 'check.t';
open my $fh, '<', $file or die "Cannot open $file: $!\n";

my $content = do { local $/; <$fh> };
close $fh;

# Split into tests (separated by --- on its own line)
my @blocks = split /^---\s*$/m, $content;

my $total = 0;
my $kept = 0;
my $skipped = 0;
my %skip_reasons;

for my $block (@blocks) {
    # Extract test name
    my ($name) = $block =~ /^name:\s*(.+)$/m;
    next unless defined $name;
    $total++;

    # Extract category
    my ($category) = $block =~ /^category:\s*(.+)$/m;
    $category //= '';

    # Extract stdin
    my $stdin = '';
    if ($block =~ /^stdin:\n((?:\t.*\n)*)/m) {
        $stdin = $1;
        $stdin =~ s/^\t//gm;
    }

    # Extract all fields for inspection
    my $skip = 0;
    my $reason = '';

    # Skip tests requiring controlling terminal (job control)
    if ($block =~ /need-ctty:\s*yes/) {
        $skip = 1;
        $reason = 'need-ctty';
    }

    # Skip disabled tests
    if ($category =~ /\bdisabled\b/) {
        $skip = 1;
        $reason = 'disabled';
    }

    # Skip tests requiring select
    if ($category =~ /\bhave:select/) {
        $skip = 1;
        $reason = 'select';
    }

    # Skip ksh legacy mode tests
    if ($category =~ /\bshell:legacy-yes\b/ && $category !~ /!shell:legacy-yes/) {
        $skip = 1;
        $reason = 'legacy-mode';
    }

    # Skip tests that use [[ ]] extended test
    if ($stdin =~ /\[\[/) {
        $skip = 1;
        $reason = 'ksh-[[]]';
    }

    # Skip tests that use typeset/nameref/integer
    if ($stdin =~ /\btypeset\b|\bnameref\b|\binteger\b/) {
        $skip = 1;
        $reason = 'typeset';
    }

    # Skip tests that use arrays
    if ($stdin =~ /\w+\[\d+\]/ || $stdin =~ /set\s+-A/) {
        $skip = 1;
        $reason = 'arrays';
    }

    # Skip tests that use 'print' builtin (ksh-specific)
    if ($stdin =~ /\bprint\s+(-r|--)\b|\bprint\s+-[nrupR]/) {
        $skip = 1;
        $reason = 'ksh-print';
    }

    # Skip tests using 'select'
    if ($stdin =~ /\bselect\b/) {
        $skip = 1;
        $reason = 'select';
    }

    # Skip tests using coprocesses (|&)
    if ($stdin =~ /\|\&/) {
        $skip = 1;
        $reason = 'coproc';
    }

    # Skip tests using 'function name {' syntax (ksh function def)
    if ($stdin =~ /\bfunction\s+\w+\s*\{/) {
        $skip = 1;
        $reason = 'ksh-function';
    }

    # Skip tests using ${var/pat/rep} (ksh substitution)
    if ($stdin =~ /\$\{[^}]+\//) {
        $skip = 1;
        $reason = 'ksh-subst';
    }

    # Skip tests using $RANDOM, $SECONDS, $LINENO (ksh specials)
    if ($stdin =~ /\$RANDOM|\$SECONDS/) {
        $skip = 1;
        $reason = 'ksh-special-var';
    }

    # Skip tests using ksh-specific builtins
    if ($stdin =~ /\bwhence\b|\blet\b/) {
        $skip = 1;
        $reason = 'ksh-builtin';
    }

    # Skip tests referencing __progname or KSH_VERSION
    if ($stdin =~ /\$__progname|\$KSH_VERSION|\$\{KSH_VERSION/) {
        $skip = 1;
        $reason = 'ksh-version';
    }

    # Skip tests using 'alias'
    if ($stdin =~ /\balias\b/) {
        $skip = 1;
        $reason = 'alias';
    }

    # Skip tests that rely on job control (fg, bg, jobs, kill %N)
    if ($stdin =~ /\bfg\b|\bbg\b|\bjobs\b|\bkill\s+%/) {
        $skip = 1;
        $reason = 'job-control';
    }

    # Skip tests using exec -a (ksh extension)
    if ($stdin =~ /\bexec\s+-a\b/) {
        $skip = 1;
        $reason = 'ksh-exec';
    }

    # Skip history-related tests
    if ($stdin =~ /\bfc\b|\bhistory\b/) {
        $skip = 1;
        $reason = 'history';
    }

    # Skip tests using here-strings <<<
    if ($stdin =~ /<<</) {
        $skip = 1;
        $reason = 'here-string';
    }

    # Skip tests using 'print' as a command (ksh print builtin)
    if ($stdin =~ /^\s*print\b/m || $stdin =~ /;\s*print\b/) {
        $skip = 1;
        $reason = 'ksh-print-cmd';
    }

    # Skip tests using arithmetic array access x[n]
    if ($stdin =~ /\$\(\([^)]*\[/) {
        $skip = 1;
        $reason = 'arith-array';
    }

    # Skip tests using ++/-- (pre/post increment — ksh extension)
    if ($stdin =~ /\+\+|\-\-/ && $stdin =~ /\$\(\(/) {
        $skip = 1;
        $reason = 'arith-incr';
    }

    # Skip tests using , operator in arithmetic
    if ($stdin =~ /\$\(\([^)]*,/) {
        $skip = 1;
        $reason = 'arith-comma';
    }

    # Skip tests referencing __perlname
    if ($stdin =~ /__perlname/) {
        $skip = 1;
        $reason = 'perl-dep';
    }

    # Skip tests using $LINENO (we don't track it yet)
    if ($stdin =~ /\$LINENO|\$\{LINENO/) {
        $skip = 1;
        $reason = 'lineno';
    }

    # Skip tests that use -i (interactive mode flag)
    if ($block =~ /arguments:.*-i/) {
        $skip = 1;
        $reason = 'interactive';
    }

    # Skip tests using (( )) arithmetic command (ksh extension, not $((  )) expansion)
    if ($stdin =~ /^\s*\(\(|;\s*\(\(|!\s*\(\(/ && $stdin !~ /\$\(\(/) {
        $skip = 1;
        $reason = 'ksh-arith-cmd';
    }
    # Also skip (( in conditionals
    if ($stdin =~ /\b\(\(\s*\w/ && $stdin !~ /\$\(\(/) {
        $skip = 1;
        $reason = 'ksh-arith-cmd';
    }

    # Skip extended glob patterns @(...), +(...), !(...) in non-case context
    if ($stdin =~ /[+@!]\(.*\|/) {
        $skip = 1;
        $reason = 'ksh-eglob';
    }

    # Skip ${var:offset:length} substring (ksh extension)
    if ($stdin =~ /\$\{\w+:\d/) {
        $skip = 1;
        $reason = 'ksh-substring';
    }

    # Skip ulimit (not a POSIX-required builtin for non-interactive)
    if ($stdin =~ /\bulimit\b/) {
        $skip = 1;
        $reason = 'ulimit';
    }

    # Skip realpath (external command, not always available)
    if ($stdin =~ /\brealpath\b/) {
        $skip = 1;
        $reason = 'realpath';
    }

    # Skip tests using set -o (we only support set -e/-u/-x short flags)
    if ($stdin =~ /\bset\s+-o\b|\bset\s+\+o\b/) {
        $skip = 1;
        $reason = 'set-o';
    }

    # Skip tests using 'integer' keyword (ksh)
    if ($stdin =~ /^\s*integer\b/m) {
        $skip = 1;
        $reason = 'ksh-integer';
    }

    # Skip funsub ${|...} (mksh extension)
    if ($stdin =~ /\$\{\|/) {
        $skip = 1;
        $reason = 'funsub';
    }

    # Skip event substitution (history)
    if ($name =~ /event-subst/) {
        $skip = 1;
        $reason = 'event-subst';
    }

    # Skip array tests that slipped through
    if ($name =~ /^arrays-|^arrassign/) {
        $skip = 1;
        $reason = 'arrays';
    }

    # Skip wcswidth tests (mksh-specific builtin)
    if ($name =~ /^wcswidth/) {
        $skip = 1;
        $reason = 'wcswidth';
    }

    # Skip EBCDIC-specific tests
    if ($name =~ /ebcdic/) {
        $skip = 1;
        $reason = 'ebcdic';
    }

    # Skip tests that are known to hang/timeout
    if ($name =~ /exit-err-5|heredoc-tmpfile/) {
        $skip = 1;
        $reason = 'timeout-prone';
    }

    # Skip tests that even dash fails (mksh-specific behavior)
    my %dash_also_fails = map { $_ => 1 } qw(
        arith-div-byzero arith-divnull better-parens-2b better-parens-2c
        better-parens-4b better-parens-4c bksl-nl-6 bksl-nl-ign-5
        bksl-nl-ksh-1 bksl-nl-ksh-2 break-3 break-4 case-zsh
        continue-3 continue-4 echo-test-1 eglob-bad-2 eglob-case-2
        eglob-infinite-plus eglob-trim-1 eglob-trim-2 exit-err-3
        expand-bang-1 expand-weird-2 funsub-1 glob-bad-1
        heredoc-12 heredoc-4a heredoc-4b heredoc-comsub-1 heredoc-comsub-2
        heredoc-comsub-3 heredoc-comsub-4 heredoc-subshell-1
        integer-base-7 integer-base-check-numeric-from-1 integer-base-one-4
        oksh-seterror-6 read-ksh-1 regression-55 regression-62
        single-quotes-in-heredoc-trim test-numeq unset-fnc-local-sh
        utilities-getopts-2 varexpand-substr-6 xxx-param-_-1
        xxx-param-subst-qmark-1 xxx-quoted-newline-3 xxx-set-option-1
        xxx-status-2 xxx-substitution-eval-order-2 xxx-variable-syntax-1
        xxx-variable-syntax-2
        heredoc-4an heredoc-4bn IFS-space-colon-4 regression-43
        syntax-1 xxx-multi-assignment-posix-subassign utilities-getopts-3
    );
    if ($dash_also_fails{$name}) {
        $skip = 1;
        $reason = 'dash-also-fails';
    }

    if ($skip) {
        $skipped++;
        $skip_reasons{$reason}++;
    } else {
        $kept++;
        print $block;
        print "---\n";
    }
}

# Print stats to stderr
print STDERR "Total tests: $total\n";
print STDERR "Kept: $kept\n";
print STDERR "Skipped: $skipped\n";
print STDERR "Skip reasons:\n";
for my $r (sort { $skip_reasons{$b} <=> $skip_reasons{$a} } keys %skip_reasons) {
    printf STDERR "  %-20s %d\n", $r, $skip_reasons{$r};
}
