"""Observe a fixture-owned proc memory FD across real vfork/exec.

The original producer and static probe are unchanged. This experiment supplies
no runtime permission, complete writer census or source/input authority.
"""
import errno
import importlib.util
import os
from pathlib import Path
import stat
import struct
import sys


def shared_address(pid,producer):
    """Locate only the original fixture's four-byte `shared` object."""
    body=producer.read_bytes()
    assert body[:6]==b'\x7fELF\x02\x01' and len(body)<1024*1024
    kind,machine=struct.unpack_from('<HH',body,16)
    assert kind in (2,3) and machine==62
    offset,=struct.unpack_from('<Q',body,40)
    size,count=struct.unpack_from('<HH',body,58)
    assert size==64 and 0<count<1024 and offset+size*count<=len(body)
    sections=[struct.unpack_from('<IIQQQQIIQQ',body,offset+i*size) for i in range(count)]
    matches=[]
    for section in sections:
        if section[1]!=2:continue # SHT_SYMTAB
        start,length,link,entry_size=section[4],section[5],section[6],section[9]
        assert entry_size==24 and length%24==0 and start+length<=len(body) and link<len(sections)
        strings=sections[link];names=body[strings[4]:strings[4]+strings[5]]
        for at in range(start,start+length,24):
            name,info,_,_,value,extent=struct.unpack_from('<IBBHQQ',body,at)
            if names[name:].split(b'\0',1)[0]==b'shared':
                assert info&15==1 and extent==4 # STT_OBJECT
                matches.append(value)
    assert len(matches)==1
    identity=producer.stat();actual=os.stat(f'/proc/{pid}/exe')
    assert stat.S_ISREG(actual.st_mode) and (actual.st_dev,actual.st_ino)==(identity.st_dev,identity.st_ino)
    mappings=[]
    for row in Path(f'/proc/{pid}/maps').read_text().splitlines():
        fields=row.split(None,5)
        major,minor=(int(x,16) for x in fields[3].split(':'))
        if int(fields[2],16)==0 and os.makedev(major,minor)==identity.st_dev and int(fields[4])==identity.st_ino:
            mappings.append(int(fields[0].split('-')[0],16))
    assert len(mappings)==1
    return matches[0]+(mappings[0] if kind==3 else 0)


def main(arguments):
    planner,supervisor,library,producer,proxy,wrong,receipt=arguments
    source=Path(__file__).with_name('ci_cargo_vfork_exec_cases.py')
    spec=importlib.util.spec_from_file_location('original_exec_memory_probe',source)
    cases=importlib.util.module_from_spec(spec);spec.loader.exec_module(cases)
    original=cases.experiment;state={};gate=[]

    def pending(parent,child):
        address=shared_address(child,Path(producer))
        old=os.open(f'/proc/{child}/mem',os.O_RDWR|os.O_CLOEXEC)
        state['old']=old
        parent_fd=os.open(f'/proc/{parent}/mem',os.O_RDWR|os.O_CLOEXEC)
        state.update(old=old,parent_fd=parent_fd,address=address,parent=parent,child=child)
        assert os.pread(old,4,address)==struct.pack('<i',41)
        try:
            assert os.pwrite(old,struct.pack('<i',42),address)==4
            assert os.pread(parent_fd,4,address)==struct.pack('<i',42)
        finally:
            assert os.pwrite(old,struct.pack('<i',41),address)==4
        assert os.pread(parent_fd,4,address)==struct.pack('<i',41)

    def executed(child):
        assert child==state['child']
        address=state['address'];old=state['old'];parent=state['parent_fd']
        current=os.open(f'/proc/{child}/mem',os.O_RDWR|os.O_CLOEXEC)
        try:
            # This address belongs to the fixture's old PIE data page, not the
            # new static probe. Verify that fact using the actual new mm.
            try:os.pread(current,4,address)
            except OSError as error:assert error.errno==errno.EIO
            else:raise AssertionError('fixture address unexpectedly mapped in new image')
            assert os.pread(old,4,address)==struct.pack('<i',41)
            try:
                assert os.pwrite(old,struct.pack('<i',99),address)==4
                assert os.pread(parent,4,address)==struct.pack('<i',99)
                try:os.pread(current,4,address)
                except OSError as error:assert error.errno==errno.EIO
                else:raise AssertionError('old FD write changed the new image mapping')
            finally:
                assert os.pwrite(old,struct.pack('<i',41),address)==4
            assert os.pread(parent,4,address)==struct.pack('<i',41)
            gate[0].proof.update(pre_exec_fd_mutated_shared_parent=True,
                post_exec_old_fd_mutated_original_parent=True,
                post_exec_reopened_child_fd_old_address_errno=errno.EIO,
                old_parent_value_restored=True,complete_writer_closure=False)
        finally:os.close(current)
        return True

    def construct(*args,**kwargs):
        assert not kwargs
        value=original(*args,inspect_pending_exec=pending,inspect_exec=executed)
        gate.append(value);return value
    cases.experiment=construct
    try:cases.main([planner,supervisor,library,producer,proxy,wrong,'finish',receipt])
    finally:
        for key in ('old','parent_fd'):
            if key in state:os.close(state[key])


if __name__=='__main__':main(sys.argv[1:])
