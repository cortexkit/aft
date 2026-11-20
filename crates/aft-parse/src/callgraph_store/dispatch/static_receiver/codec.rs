use aft_store::content::{
    codec::{Decoder, Encoder},
    DecodeError,
};

use super::{Declaration, Receiver, Scope, StaticFacts};

impl StaticFacts {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::default();
        out.usize(self.scopes.len());
        for scope in &self.scopes {
            out.str(&scope.namespace);
            out.strs(&scope.imports);
            encode_pairs(&mut out, &scope.aliases);
        }
        out.usize(self.types.len());
        for ty in &self.types {
            out.str(&ty.name);
            out.u32(ty.scope);
            out.strs(&ty.bases);
            out.strs(&ty.values);
            encode_pairs(&mut out, &ty.methods);
        }
        out.usize(self.receivers.len());
        for receiver in &self.receivers {
            out.str(&receiver.name);
            out.u32(receiver.scope);
            out.u32(receiver.owner.map_or(0, |id| id + 1));
            out.opt_str(receiver.module.as_deref());
        }
        out.usize(self.sites.len());
        for &(ordinal, id) in &self.sites {
            out.u32(ordinal);
            out.u32(id);
        }
        out.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(bytes)?;
        let scopes = read_list(&mut input, |i| {
            Ok(Scope {
                namespace: i.string()?,
                imports: i.strings()?,
                aliases: read_pairs(i)?,
            })
        })?;
        let types = read_list(&mut input, |i| {
            Ok(Declaration {
                name: i.string()?,
                scope: index(i, scopes.len())?,
                bases: i.strings()?,
                values: i.strings()?,
                methods: read_pairs(i)?,
            })
        })?;
        let receivers = read_list(&mut input, |i| {
            let name = i.string()?;
            let scope = index(i, scopes.len())?;
            let owner = i.u32()?.checked_sub(1);
            if owner.is_some_and(|id| id as usize >= types.len()) {
                return Err(DecodeError("static receiver owner"));
            }
            let module = i.opt_string()?;
            Ok(Receiver {
                name,
                scope,
                owner,
                module,
            })
        })?;
        let sites = read_list(&mut input, |i| Ok((i.u32()?, index(i, receivers.len())?)))?;
        input.finish()?;
        if sites.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(DecodeError("static receiver ordinals are not in order"));
        }
        Ok(Self {
            types,
            scopes,
            receivers,
            sites,
        })
    }
}

fn index(input: &mut Decoder<'_>, len: usize) -> Result<u32, DecodeError> {
    let id = input.u32()?;
    if id as usize >= len {
        return Err(DecodeError("static receiver fact index"));
    }
    Ok(id)
}

fn read_list<T>(
    input: &mut Decoder<'_>,
    mut read: impl FnMut(&mut Decoder<'_>) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    let count = input.count()?;
    (0..count).map(|_| read(input)).collect()
}

fn read_pairs(input: &mut Decoder<'_>) -> Result<Vec<(String, String)>, DecodeError> {
    read_list(input, |i| Ok((i.string()?, i.string()?)))
}

fn encode_pairs(out: &mut Encoder, pairs: &[(String, String)]) {
    out.usize(pairs.len());
    for (name, value) in pairs {
        out.str(name);
        out.str(value);
    }
}
