# Prints every node of a plan with Pg::Explain's inclusive and exclusive times.
use strict;
use warnings;
use Pg::Explain;
local $/;
open my $fh, '<', $ARGV[0] or die;
my $source = <$fh>;
my $explain = Pg::Explain->new( source => $source );
my @queue = ( $explain->top_node );
while ( my $node = shift @queue ) {
    printf "%s\t%s\t%s\t%.6f\t%.6f\t%s\n",
        $node->actual_time_last // '', $node->actual_rows // '', $node->actual_loops // '',
        $node->total_exclusive_time // -1, $node->total_inclusive_time // -1, $node->type;
    push @queue, @{ $node->initplans || [] }, @{ $node->sub_nodes || [] }, @{ $node->subplans || [] }, values %{ $node->ctes || {} };
}
