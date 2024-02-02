use std::collections::HashMap;

use crate::{
    extension::{units::Units, ExtenderUnit},
    Hasher, NodeCount, NodeIndex, NodeMap, Round,
};

fn common_vote(relative_round: Round) -> bool {
    // This should only be called for relative round >= 2, so to be precise we start with true, false, true, and then
    if relative_round == 3 {
        return false;
    }
    if relative_round <= 4 {
        return true;
    }
    // we alternate between true and false starting from true in round 5.
    relative_round % 2 == 1
}

enum VoteError<H: Hasher> {
    EliminateCandidate,
    ElectionDone(H::Hash),
}

struct CandidateElection<H: Hasher> {
    round: Round,
    candidate_creator: NodeIndex,
    candidate_hash: H::Hash,
    votes: HashMap<H::Hash, bool>,
}

impl<H: Hasher> CandidateElection<H> {
    /// Creates an election for the given candidate.
    /// The candidate will eventually either get elected or eliminated.
    pub fn new(candidate: &ExtenderUnit<H>) -> Self {
        CandidateElection {
            round: candidate.round,
            candidate_creator: candidate.creator,
            candidate_hash: candidate.hash,
            votes: HashMap::new(),
        }
    }

    fn parent_votes(
        &mut self,
        parents: &NodeMap<H::Hash>,
        units: &Units<H>,
    ) -> Result<(NodeCount, NodeCount), VoteError<H>> {
        let (mut votes_for, mut votes_against) = (NodeCount(0), NodeCount(0));
        for parent in parents.values() {
            // Since this code is being called round-by-round we should only ever hit the cache here.
            // In particular this should always return `Ok`, but this way the code is fine even if
            // someone were to call it in a different order.
            match self.vote(*parent, units)? {
                true => votes_for += NodeCount(1),
                false => votes_against += NodeCount(1),
            }
        }
        Ok((votes_for, votes_against))
    }

    fn vote_from_parents(
        &mut self,
        parents: &NodeMap<H::Hash>,
        relative_round: Round,
        units: &Units<H>,
    ) -> Result<bool, VoteError<H>> {
        use VoteError::*;
        let threshold = (parents.size() * 2) / 3 + NodeCount(1);
        // Gather parents' votes.
        let (votes_for, votes_against) = self.parent_votes(parents, units)?;
        assert!(votes_for + votes_against >= threshold);
        let common_vote = common_vote(relative_round);
        // If the round is sufficiently high we are done voting for the candidate if
        if relative_round >= 3 {
            match common_vote {
                // the default vote is for the candidate and the parents' votes are for over the threshold,
                true if votes_for >= threshold => return Err(ElectionDone(self.candidate_hash)),
                // or the default vote is against the candidate and the parents' votes are against over the threshold.
                false if votes_against >= threshold => return Err(EliminateCandidate),
                _ => (),
                // Note that this means the earliest we can have a head elected is round 4.
            }
        }

        // The vote is either identical to all the votes of the parents, or the default vote if that is not possible.
        Ok(match (votes_for, votes_against) {
            (NodeCount(0), _) => false,
            (_, NodeCount(0)) => true,
            _ => common_vote,
        })
    }

    fn compute_vote(
        &mut self,
        voter: &ExtenderUnit<H>,
        units: &Units<H>,
    ) -> Result<bool, VoteError<H>> {
        // This should never happen, but this preserves correctness even if someone calls this for too old units.
        if voter.round <= self.round {
            return Ok(false);
        }
        let relative_round = voter.round - self.round;
        // Direct descendands vote for, all other units of that round against.
        if relative_round == 1 {
            return Ok(voter.parents.get(self.candidate_creator) == Some(&self.candidate_hash));
        }
        // Otherwise we compute the vote based on the parents' votes.
        self.vote_from_parents(&voter.parents, relative_round, units)
    }

    fn vote(&mut self, voter: H::Hash, units: &Units<H>) -> Result<bool, VoteError<H>> {
        // Check the vote cache.
        if let Some(vote) = self.votes.get(&voter) {
            return Ok(*vote);
        }
        let voter = units
            .get(&voter)
            .expect("only requesting votes from units we have");
        let result = self.compute_vote(voter, units)?;
        self.votes.insert(voter.hash, result);
        Ok(result)
    }

    /// Add a single voter and copute their vote. This might end up electing or eliminating the candidate.
    pub fn add_voter(mut self, voter: H::Hash, units: &Units<H>) -> Result<Self, VoteError<H>> {
        self.vote(voter, units).map(|_| self)
    }

    /// Compute all the votes for units we have at the moment. This might end up electing or eliminating the candidate.
    pub fn compute_votes(mut self, units: &Units<H>) -> Result<Self, VoteError<H>> {
        for round in self.round + 1..=units.highest_round() {
            for voter in units.in_round(round).expect("units come in order") {
                self.vote(voter, units)?;
            }
        }
        Ok(self)
    }
}

/// Election for a single round.
pub struct RoundElection<H: Hasher> {
    // Remaining candidates for this round's head, in reverese order.
    candidates: Vec<H::Hash>,
    voting: CandidateElection<H>,
}

/// An election result.
pub enum ElectionResult<H: Hasher> {
    /// The election is not done yet.
    Pending(RoundElection<H>),
    /// The head has been elected.
    Elected(H::Hash),
}

impl<H: Hasher> RoundElection<H> {
    /// Create a new round election. It might immediately be decided, so this might return an election result rather than a pending election.
    /// Returns an error when it's too early to finalize the candidate list, i.e. we are not at least 3 rounds ahead of the election round.
    pub fn for_round(round: Round, units: &Units<H>) -> Result<ElectionResult<H>, ()> {
        // If we don't yet have a unit of round + 3 we might not know about the winning candidate, so we cannot start the election.
        if units.highest_round() < round + 3 {
            return Err(());
        }
        // We might be missing units from this round, but they are guaranteed to get consistently eliminated.
        let mut candidates = units
            .in_round(round)
            .expect("units come in order, so we definitely have units from this round");
        candidates.sort();
        // We will be `pop`ing the candidates from the back.
        candidates.reverse();
        let candidate = units
            .get(&candidates.pop().expect("there is a candidate"))
            .expect("we have all the units we work with");
        let voting = CandidateElection::new(candidate);
        let election = RoundElection { candidates, voting };
        // We might already have enough units to pick a head.
        Ok(election.compute_votes(units))
    }

    fn handle_candidate_election_result(
        result: Result<CandidateElection<H>, VoteError<H>>,
        mut candidates: Vec<H::Hash>,
        units: &Units<H>,
    ) -> ElectionResult<H> {
        use ElectionResult::*;
        use VoteError::*;
        match result {
            // Wait for more voters.
            Ok(voting) => Pending(RoundElection { candidates, voting }),
            // Pick the next candidate and keep trying.
            Err(EliminateCandidate) => {
                let candidate = units
                    .get(&candidates.pop().expect("there is a candidate"))
                    .expect("we have all the units we work with");
                let voting = CandidateElection::new(candidate);
                RoundElection { candidates, voting }.compute_votes(units)
            }
            // Yay, we picked a head.
            Err(ElectionDone(head)) => Elected(head),
        }
    }

    fn compute_votes(self, units: &Units<H>) -> ElectionResult<H> {
        let RoundElection { candidates, voting } = self;
        Self::handle_candidate_election_result(voting.compute_votes(units), candidates, units)
    }

    /// Add a single voter to the election.
    pub fn add_voter(self, voter: H::Hash, units: &Units<H>) -> ElectionResult<H> {
        let RoundElection { candidates, voting } = self;
        Self::handle_candidate_election_result(voting.add_voter(voter, units), candidates, units)
    }
}

#[cfg(test)]
mod test {
    use crate::{
        extension::{
            election::{ElectionResult, RoundElection},
            tests::{construct_unit, construct_unit_all_parents},
            units::Units,
        },
        NodeCount, NodeIndex,
    };
    use aleph_bft_mock::Hasher64;

    #[test]
    fn refuses_to_elect_without_units() {
        let units = Units::<Hasher64>::new();
        assert!(RoundElection::for_round(0, &units).is_err());
    }

    #[test]
    fn refuses_to_elect_with_insufficient_units() {
        let mut units = Units::new();
        let n_members = NodeCount(4);
        let max_round = 2;
        for round in 0..=max_round {
            for creator in n_members.into_iterator() {
                units.add_unit(construct_unit_all_parents(creator, round, n_members));
            }
        }
        assert!(RoundElection::for_round(0, &units).is_err());
    }

    #[test]
    fn easy_election() {
        use ElectionResult::*;
        let mut units = Units::new();
        let n_members = NodeCount(4);
        let max_round = 3;
        for round in 0..=max_round {
            for creator in n_members.into_iterator() {
                units.add_unit(construct_unit_all_parents(creator, round, n_members));
            }
        }
        let election = RoundElection::for_round(0, &units).expect("we have enough rounds");
        let election = match election {
            Pending(election) => election,
            Elected(_) => panic!("elected head without units of round + 4"),
        };
        let last_voter = construct_unit_all_parents(NodeIndex(0), 4, n_members);
        units.add_unit(last_voter.clone());
        match election.add_voter(last_voter.hash, &units) {
            Pending(_) => panic!("failed to elect obvious head"),
            Elected(head) => {
                assert_eq!(units.get(&head).expect("we have the head").round, 0);
            }
        }
    }

    #[test]
    fn immediate_election() {
        use ElectionResult::*;
        let mut units = Units::new();
        let n_members = NodeCount(4);
        let max_round = 4;
        for round in 0..=max_round {
            for creator in n_members.into_iterator() {
                units.add_unit(construct_unit_all_parents(creator, round, n_members));
            }
        }
        let election = RoundElection::for_round(0, &units).expect("we have enough rounds");
        match election {
            Pending(_) => panic!("should have elected"),
            Elected(head) => {
                assert_eq!(units.get(&head).expect("we have the head").round, 0);
            }
        }
    }

    #[test]
    fn eliminates_unpopular() {
        use ElectionResult::*;
        let mut units = Units::new();
        let n_members = NodeCount(4);
        let max_round = 4;
        for creator in n_members.into_iterator() {
            units.add_unit(construct_unit_all_parents(creator, 0, n_members));
        }
        let mut candidate_hashes = units.in_round(0).expect("just added these");
        candidate_hashes.sort();
        let skipped_parent = units
            .get(&candidate_hashes[0])
            .expect("we just got it")
            .creator;
        let active_nodes: Vec<_> = n_members
            .into_iterator()
            .filter(|parent_id| parent_id != &skipped_parent)
            .collect();
        for round in 1..=max_round {
            for creator in &active_nodes {
                units.add_unit(construct_unit(
                    *creator,
                    round,
                    active_nodes
                        .iter()
                        .map(|parent_id| (*parent_id, round - 1))
                        .collect(),
                    n_members,
                ));
            }
        }
        let election = RoundElection::for_round(0, &units).expect("we have enough rounds");
        match election {
            Pending(_) => panic!("should have elected"),
            Elected(head) => {
                // This should be the second unit in order, as the first was not popular.
                assert_eq!(head, candidate_hashes[1]);
            }
        }
    }
}
