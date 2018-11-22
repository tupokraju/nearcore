mod message;
mod reporter;

use primitives::types::*;
use primitives::traits::{WitnessSelector, Payload};

use std::collections::{HashSet, HashMap, VecDeque};

use self::message::Message;
use self::reporter::{MisbehaviourReporter, ViolationType};
use typed_arena::Arena;

/// The data-structure of the TxFlow DAG that supports adding messages and updating counters/flags,
/// but does not support communication-related logic. Also does verification of the messages
/// received from other nodes.
/// It uses unsafe code to implement a self-referential struct and the interface makes sure that
/// the references never outlive the instances.
pub struct DAG<'a, P: 'a + Payload, W: 'a + WitnessSelector> {
    /// UID of the node.
    owner_uid: UID,
    arena: Arena<Box<Message<'a, P>>>,
    /// Stores all messages known to the current root.
    messages: HashSet<&'a Message<'a, P>>,
    /// Stores all current roots.
    roots: HashSet<&'a Message<'a, P>>,

    witness_selector: &'a W,
    starting_epoch: u64,

    misbehaviour: MisbehaviourReporter,
    participant_head: HashMap<UID, StructHash>,
}

impl<'a, P: 'a + Payload, W:'a+ WitnessSelector> DAG<'a, P, W> {
    pub fn new(owner_uid: UID, starting_epoch: u64, witness_selector: &'a W) -> Self {
        DAG {
            owner_uid,
            arena: Arena::new(),
            messages: HashSet::new(),
            roots: HashSet::new(),
            witness_selector,
            starting_epoch,
            misbehaviour: MisbehaviourReporter::new(),
            participant_head: HashMap::new(),
        }
    }

    fn find_fork(&self, message: &Message<'a, P>) -> Option<StructHash> {
        let uid = message.data.body.owner_uid.clone();

        if let Some(last_hash) = self.participant_head.get(&uid) {
            let mut visited = HashSet::new();
            let mut queue = VecDeque::new();

            for par in &message.parents {
                visited.insert(par.computed_hash);
                queue.push_back(par.clone());
            }

            // Run BFS to detect if this message sees last message of
            // participant uid. In case of forks this BFS will explore almost
            // entire DAG stopping at previous messages from participant uid.
            // TODO: Prune this BFS (maybe change algorithm to detect forks)
            while queue.len() > 0 {
                let cur = queue.pop_front();

                if let Some(cur_message) = cur {
                    if cur_message.data.body.owner_uid == uid {
                        if cur_message.computed_hash == *last_hash {
                            // target message found
                            return None;
                        }
                        else {
                            // skip messages from participant uid
                            continue;
                        }
                    }
                    else {
                        if visited.contains(&cur_message.computed_hash) {
                            // skip messages already visited
                            continue;
                        }
                        else{
                            // mark message as visited
                            visited.insert(cur_message.computed_hash);
                            queue.push_back(cur_message.clone());
                        }
                    }
                }
            }

            // If message not found at this point it means it is a fork
            Some(last_hash.clone())
        }
        else {
            None
        }
    }

    /// Whether there is one root only and it was created by the current owner.
    pub fn is_current_owner_root(&self) -> bool {
        self.current_root_data()
            .map(|d| d.body.owner_uid == self.owner_uid)
            .unwrap_or(false)
    }

    /// Return true if there are several roots.
    pub fn has_dangling_roots(&self) -> bool {
        self.roots.len() > 1
    }

    /// If there is one root it returns its data.
    pub fn current_root_data(&self) -> Option<&SignedMessageData<P>> {
        if self.roots.len() == 1 {
            self.roots.iter().next().map(|m| &m.data)
        } else {
            None
        }
    }

    pub fn contains_message(&self, hash: &StructHash) -> bool {
        self.messages.contains(hash)
    }

    /// Create a copy of the message data from the dag given hash.
    pub fn copy_message_data_by_hash(&self, hash: &StructHash) -> Option<SignedMessageData<P>> {
       self.messages.get(hash).map(|m| m.data.clone())
    }

    /// Verify that this message does not violate the protocol.
    fn verify_message(&mut self, message: &Message<'a, P>) -> Result<(), &'static str> {
        // Check epoch
        if message.computed_epoch != message.data.body.epoch {
            let mb = ViolationType::BadEpoch {
                message: message.computed_hash.clone()
            };

            self.misbehaviour.report(mb);
        }

        let fork_message = self.find_fork(message);

        if let Some(fork_message_hash) = fork_message {
            let mb = ViolationType::ForkAttempt {
                message_0: fork_message_hash,
                message_1: message.computed_hash.clone()
            };

            self.misbehaviour.report(mb);
        }

        // TODO: Check correct signature

        Ok({})
    }

    // Takes ownership of the message.
    pub fn add_existing_message(&mut self, message_data: SignedMessageData<P>) -> Result<(), &'static str> {
        // Check whether this is a new message.
        if self.messages.contains(&message_data.hash) {
            return Ok({})
        }

        // Wrap message data and connect to the parents so that the verification can be run.
        let mut message = Box::new(Message::new(message_data));
        let parent_hashes:Vec<StructHash> = message.data.body.parents.iter().cloned().collect();

        for p_hash in parent_hashes {
            if let Some(&p) = self.messages.get(&p_hash) {
                message.parents.insert(p);
            } else {
                return Err("Some parents of the message are unknown");
            }
        }

        // Compute epochs, endorsements, etc.
        message.init(true, self.starting_epoch, self.witness_selector);

        // Verify the message.
        if let Err(e) = self.verify_message(&message) {
            return Err(e)
        }

        // Finally, take ownership of the message and update the roots.
        for p in &message.parents {
            self.roots.remove(p);
        }

        self.participant_head.insert(message.data.body.owner_uid, message.computed_hash);
        let message_ptr = self.arena.alloc(message).as_ref() as *const Message<'a, P>;
        self.messages.insert(unsafe{&*message_ptr});
        self.roots.insert(unsafe{&*message_ptr});
        Ok({})
    }

    /// Creates a new message that points to all existing roots. Takes ownership of the payload and
    /// the endorsements.
    pub fn create_root_message(&mut self, payload: P, endorsements: Vec<Endorsement>) -> &'a Message<'a, P> {
        let mut message = Box::new(Message::new(
            SignedMessageData {
                owner_sig: 0,  // Will populate once the epoch is computed.
                hash: 0,  // Will populate once the epoch is computed.
                body: MessageDataBody {
                    owner_uid: self.owner_uid,
                    parents: (&self.roots).into_iter().map(|m| m.computed_hash).collect(),
                    epoch: 0,  // Will be computed later.
                    payload,
                    endorsements,
                }
            }
        ));
        message.init(true, self.starting_epoch, self.witness_selector);
        message.assume_computed_hash_epoch();

        // Finally, take ownership of the new root.
        let message_ptr = self.arena.alloc(message).as_ref() as *const Message<'a, P>;
        self.messages.insert(unsafe { &*message_ptr });
        self.roots.clear();
        self.roots.insert(unsafe { &*message_ptr });
        unsafe { &*message_ptr }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, HashMap};
    use typed_arena::Arena;

    struct FakeWitnessSelector {
        schedule: HashMap<u64, HashSet<UID>>,
    }

    impl FakeWitnessSelector {
        fn new() -> FakeWitnessSelector {
            FakeWitnessSelector {
                schedule: map!{
               0 => set!{0, 1, 2, 3}, 1 => set!{1, 2, 3, 4},
               2 => set!{2, 3, 4, 5}, 3 => set!{3, 4, 5, 6}}
            }
        }
    }

    impl WitnessSelector for FakeWitnessSelector {
        fn epoch_witnesses(&self, epoch: u64) -> &HashSet<u64> {
            self.schedule.get(&epoch).unwrap()
        }
        fn epoch_leader(&self, epoch: u64) -> UID {
            *self.epoch_witnesses(epoch).iter().min().unwrap()
        }
        fn random_witness(&self, epoch: u64) -> u64 {
            unimplemented!()
        }
    }

    #[test]
    fn check_correct_epoch_simple(){
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);

        // Parent have greater epoch than children
        let (a, b);
        simple_bare_messages!(data_arena, all_messages [[1, 2 => a;] => 1, 1 => b;]);

        assert!(dag.add_existing_message((*a).clone()).is_ok());
        assert!(dag.add_existing_message((*b).clone()).is_ok());

        for message in &dag.messages{
            assert_eq!(message.computed_epoch, 0);
        }

        // Both messages have invalid epoch number so two reports were made
        assert_eq!(dag.misbehaviour.violations.len(), 2);

        for violation in &dag.misbehaviour.violations {
            if let ViolationType::BadEpoch { message: _ } = violation {
                // expected violation type
            }
            else {
                assert!(false);
            }
        }
    }

    #[test]
    fn check_correct_epoch_complex(){
        // When a message can have epoch k, but since it doesn't have messages
        // with smaller epochs it creates them.

        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);

        let a;
        simple_bare_messages!(data_arena, all_messages [[0, 0; 1, 0; 2, 0;] => 0, 1 => a;]);
        simple_bare_messages!(data_arena, all_messages [[=> a;] => 3, 1;]);

        for m in &all_messages {
            assert!(dag.add_existing_message((*m).clone()).is_ok());
        }

        for message in &dag.messages{
            assert_eq!(message.computed_epoch, message.data.body.epoch);
        }

        assert_eq!(dag.misbehaviour.violations.len(), 0);
    }

    #[test]
    fn notice_simple_fork() {
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);

        simple_bare_messages!(data_arena, all_messages [[0, 0; 1, 0;] => 3, 1;]);
        simple_bare_messages!(data_arena, all_messages [[2, 0; 1, 0;] => 3, 1;]);

        for m in &all_messages {
            assert!(dag.add_existing_message((*m).clone()).is_ok());
        }

        assert_eq!(dag.misbehaviour.violations.len(), 1);

        let violation = dag.misbehaviour.violations.get(0usize);

        // similar to: assert(isinstance(violation, ForkAttempt))
        match violation {
            Some(ViolationType::ForkAttempt{message_0 : _, message_1: _}) => {
                assert!(true);
            },
            _ => {
                assert!(false);
            },
        }
    }

    #[test]
    fn feed_complex_topology() {
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);
        let (a, b);
        simple_bare_messages!(data_arena, all_messages [[0, 0 => a; 1, 2;] => 2, 3 => b;]);
        simple_bare_messages!(data_arena, all_messages [[=> a; 3, 4;] => 4, 5;]);
        simple_bare_messages!(data_arena, all_messages [[=> a; => b; 0, 0;] => 4, 3;]);

        // Feed messages in DFS order which ensures that the parents are fed before the children.
        for m in all_messages {
            assert!(dag.add_existing_message((*m).clone()).is_ok());
        }
    }

    #[test]
    fn check_missing_messages_as_feeding() {
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);
        let (a, b, c, d, e);
        simple_bare_messages!(data_arena, all_messages [[0, 0 => a; 1, 2 => b;] => 2, 3 => c;]);
        simple_bare_messages!(data_arena, all_messages [[=> a; 3, 4 => d;] => 4, 5 => e;]);
        assert!(dag.add_existing_message((*a).clone()).is_ok());
        // Check we cannot add message e yet, because it's parent d was not received, yet.
        assert!(dag.add_existing_message((*e).clone()).is_err());
        assert!(dag.add_existing_message((*d).clone()).is_ok());
        // Check that we have two dangling roots now.
        assert_eq!(dag.roots.len(), 2);
        // Now we can add message e, because we know all its parents!
        assert!(dag.add_existing_message((*e).clone()).is_ok());
        // Check that there is only one root now.
        assert_eq!(dag.roots.len(), 1);
        // Still we cannot add message c, because b is missing.
        assert!(dag.add_existing_message((*c).clone()).is_err());
        // Now add b and c.
        assert!(dag.add_existing_message((*b).clone()).is_ok());
        assert!(dag.add_existing_message((*c).clone()).is_ok());
        // Check that we again have to dangling roots -- e and c.
        assert_eq!(dag.roots.len(), 2);
    }

    #[test]
    fn create_roots() {
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);
        let (a, b, c, d, e);
        simple_bare_messages!(data_arena, all_messages [[0, 0 => a; 1, 2 => b;] => 2, 3 => c;]);

        assert!(dag.add_existing_message((*a).clone()).is_ok());
        let message = dag.create_root_message(::testing_utils::FakePayload{}, vec![]);
        d = &message.data;

        simple_bare_messages!(data_arena, all_messages [[=> b; => d;] => 4, 5 => e;]);

        // Check that we cannot message e, because b was not added yet.
        assert!(dag.add_existing_message((*e).clone()).is_err());

        assert!(dag.add_existing_message((*b).clone()).is_ok());
        assert!(dag.add_existing_message((*e).clone()).is_ok());
        assert!(dag.add_existing_message((*c).clone()).is_ok());
    }

    // Test whether our implementation of a self-referential struct is movable.
    #[test]
    fn movable() {
        let data_arena = Arena::new();
        let selector = FakeWitnessSelector::new();
        let mut dag = DAG::new(0, 0, &selector);
        let (a, b);
        // Add some messages.
        {
            let mut all_messages = vec![];
            simple_bare_messages!(data_arena, all_messages [[0, 0 => a; 1, 2;] => 2, 3 => b;]);
            simple_bare_messages!(data_arena, all_messages [[=> a; => b; 0, 0;] => 4, 3;]);
            for m in all_messages {
                assert!(dag.add_existing_message((*m).clone()).is_ok());
            }
        }
        // Move the DAG.
        let mut moved_dag = dag;
        // And add some more messages.
        {
            let mut all_messages = vec![];
            simple_bare_messages!(data_arena, all_messages [[=> a; => b; 0, 0;] => 4, 3;]);
            for m in all_messages {
                assert!(moved_dag.add_existing_message((*m).clone()).is_ok());
            }
        }
    }

    #[test]
    fn correct_signature() {
        let selector = FakeWitnessSelector::new();
        let data_arena = Arena::new();
        let mut all_messages = vec![];
        let mut dag = DAG::new(0, 0, &selector);
        let (a, b);
        simple_bare_messages!(data_arena, all_messages [[0, 0 => a; 1, 2;] => 2, 3 => b;]);
        simple_bare_messages!(data_arena, all_messages [[=> a; 3, 4;] => 4, 5;]);
        simple_bare_messages!(data_arena, all_messages [[=> a; => b; 0, 0;] => 4, 3;]);

        // Feed messages in DFS order which ensures that the parents are fed before the children.
        for m in all_messages {
            dag.add_existing_message((*m).clone());
        }

        for m in dag.messages {
            assert_eq!(m.computed_signature, m.data.owner_sig);
        }
    }
}
