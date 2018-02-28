use std::io;
use accountant::Accountant;
use log::{PublicKey, Signature};
//use serde::Serialize;

pub struct AccountantSkel {
    pub obj: Accountant,
}

#[derive(Serialize, Deserialize)]
pub enum Request {
    Deposit {
        key: PublicKey,
        val: u64,
        sig: Signature,
    },
    Transfer {
        from: PublicKey,
        to: PublicKey,
        val: u64,
        sig: Signature,
    },
    GetBalance {
        key: PublicKey,
    },
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    Balance { key: PublicKey, val: u64 },
}

impl AccountantSkel {
    pub fn process_message(self: &mut Self, msg: Request) -> Option<Response> {
        match msg {
            Request::Deposit { key, val, sig } => {
                let _ = self.obj.deposit_signed(key, val, sig);
                None
            }
            Request::Transfer { from, to, val, sig } => {
                let _ = self.obj.transfer_signed(from, to, val, sig);
                None
            }
            Request::GetBalance { key } => {
                let val = self.obj.get_balance(&key).unwrap();
                Some(Response::Balance { key, val })
            }
        }
    }

    /// TCP Server that forwards messages to Accountant methods.
    pub fn serve(self: &mut Self, addr: &str) -> io::Result<()> {
        use std::net::TcpListener;
        use std::io::{Read, Write};
        use bincode::{deserialize, serialize};
        let listener = TcpListener::bind(addr)?;
        let mut buf = vec![];
        loop {
            let (mut stream, addr) = listener.accept()?;
            println!("connection received from {}", addr);

            // TODO: Guard against large message DoS attack.
            stream.read_to_end(&mut buf)?;

            // TODO: Return a descriptive error message if deserialization fails.
            let msg = deserialize(&buf).unwrap();
            if let Some(resp) = self.process_message(msg) {
                stream.write(&serialize(&resp).unwrap())?;
            }
        }
    }
}
